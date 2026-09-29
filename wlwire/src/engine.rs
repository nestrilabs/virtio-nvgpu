// SPDX-License-Identifier: Apache-2.0
//! One end of one proxied connection: everything between a local Wayland
//! socket and the channel to the other side of the VM boundary.
//!
//! The same engine runs on both sides -- in the guest daemon, facing a guest
//! client, and in the backend, facing the host compositor -- and, in export
//! mode, with the roles of client and server swapped. What differs is
//! configured ([`Side`], [`Local`]) or supplied per call ([`Platform`], for the
//! two descriptor kinds only a kernel or the backend's handle table can make).
//!
//! Every message in either direction is parsed against the generated tables:
//! the target object must exist and its opcode be known at the object's
//! version, or the connection ends with a protocol error. That is not
//! pedantry: descriptors travel beside the byte stream and are consumed by
//! signature, so a message the proxy cannot parse is a message whose
//! descriptors it cannot count. Parsing also drives the object table (see
//! `objects.rs`), the registry filter (see `policy.rs`), the per-class
//! descriptor translation, shm tracking, and the few value rewrites.
//!
//! Object ids are never translated and the proxy never creates objects: the
//! only messages it originates are `wl_display.error` toward a local client
//! that broke the rules, and `wp_drm_lease_device_v1.released`, which the
//! protocol promises and Hyprland 0.56 never sends.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::blob::{BlobJob, Blobs};
use crate::frame::{self, Desc, DescOut, Hello, Unit, record};
use crate::job::Job;
use crate::localout::LocalOut;
use crate::objects::{ObjError, Objects};
use crate::policy::Policy;
use crate::proto::{self, ArgKind, Dir, FdKind, IfaceId, RewriteKind, iface, op};
use crate::shm::{Shm, ShmBudget, SyncJob};
use crate::stream::{ByteBudget, Interest, Streams};
use crate::sys;
use crate::wire::{self, At, MAX_MSG, MsgBuilder, Val, peek_header, put_word};

/// `wl_display.error` codes.
pub const ERR_INVALID_OBJECT: u32 = 0;
pub const ERR_INVALID_METHOD: u32 = 1;
pub const ERR_NO_MEMORY: u32 = 2;
pub const ERR_IMPLEMENTATION: u32 = 3;
/// `wp_linux_drm_syncobj_manager_v1.error.invalid_timeline`.
pub const ERR_SYNCOBJ_INVALID_TIMELINE: u32 = 1;
/// `wl_shm.error.invalid_fd`.
pub const ERR_SHM_INVALID_FD: u32 = 2;
/// `wl_shm.error.invalid_stride`, which libwayland also posts for a pool
/// size that is not positive.
pub const ERR_SHM_INVALID_STRIDE: u32 = 1;

const CLOCK_MONOTONIC: u32 = 1;
const CLOCK_MONOTONIC_RAW: u32 = 4;

/// A WAYLAND record is closed at this many bytes or descriptors.
const WL_REC_BYTES: usize = 16 * 1024;
const WL_REC_DESCS: usize = 28;

/// Bytes queued for the channel past which a local client's input is left
/// unread ([`Engine::from_local`]): the default for an engine facing a
/// client. Counted with what commits and blobs still have to read, which
/// costs nothing until read but is what the channel will have to carry.
pub const CHANNEL_HIGH_WATER: usize = 4 << 20;

/// Something queued for the channel: a record ready to go, or the rest of a
/// commit's copy or of a blob, read as the channel takes it.
enum Out {
    Unit(Unit),
    Shm(SyncJob),
    Blob(BlobJob),
}

impl Out {
    /// Bytes this will put on the channel (a job's payload bytes).
    fn bytes(&self) -> usize {
        match self {
            Out::Unit(u) => u.bytes(),
            Out::Shm(j) => j.remaining() as usize,
            Out::Blob(j) => j.remaining() as usize,
        }
    }

    fn job_mut(&mut self) -> Option<&mut dyn Job> {
        match self {
            Out::Unit(_) => None,
            Out::Shm(j) => Some(j),
            Out::Blob(j) => Some(j),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Guest,
    Host,
}

/// What the local socket's peer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Local {
    /// A client (the guest daemon normally; the backend in export mode).
    Client,
    /// A compositor (the backend normally; the guest daemon in export mode).
    Server,
}

/// Where a fatal error came from, which decides who is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blame {
    /// The local peer broke the protocol.
    Local,
    /// The channel carried something malformed.
    Channel,
    /// The far side ended the connection with an ERROR record.
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fatal {
    pub object: u32,
    pub code: u32,
    /// Printable and at most [`MAX_FATAL_TEXT`] bytes, whatever a peer put
    /// in it (`printable`).
    pub message: String,
    pub blame: Blame,
}

/// The longest error text the engine makes, or takes from the far side.
pub const MAX_FATAL_TEXT: usize = 512;

/// `s` as it may be shown to a person: control characters (a terminal's
/// escape sequences, a newline starting a fake log line) and the invisible
/// formatting ones (bidirectional overrides) escaped, and cut to `max`
/// bytes. Error text carries what a peer sent -- an interface name it bound,
/// the far side's ERROR record verbatim -- and ends up in logs, and in
/// `wl_display.error` to a client that prints it.
pub fn printable(s: &str, max: usize) -> String {
    let mut o = String::with_capacity(s.len().min(max));
    for c in s.chars() {
        let hidden = c.is_control()
            || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}');
        let before = o.len();
        if hidden {
            o.extend(c.escape_default());
        } else {
            o.push(c);
        }
        if o.len() > max {
            o.truncate(before);
            o.push_str("...");
            break;
        }
    }
    o
}

impl Fatal {
    /// `message` is made printable here, whoever made it.
    pub fn new(blame: Blame, object: u32, code: u32, message: impl Into<String>) -> Self {
        Self {
            object,
            code,
            message: printable(&message.into(), MAX_FATAL_TEXT),
            blame,
        }
    }

    /// `wl_display.error` for a local client.
    pub fn display_error(&self) -> Vec<u8> {
        let object = if self.object == 0 { 1 } else { self.object };
        MsgBuilder::new(1, op::wl_display::EVT_ERROR)
            .object(object)
            .uint(self.code)
            .string(Some(&self.message))
            .finish()
    }

    /// An ERROR record for the far side.
    pub fn record(&self) -> Unit {
        let mut m = self.message.as_bytes().to_vec();
        m.truncate(1024);
        Unit {
            rec: record(frame::REC_ERROR, self.object, self.code, &m),
            descs: Vec::new(),
        }
    }
}

/// One entry of the guest kernel's device map: the same DRM node as the host
/// and the guest number it. `(major, minor)` pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevPair {
    pub host: (u32, u32),
    pub guest: (u32, u32),
}

/// glibc's `dev_t` encoding, which is what `st_rdev` is and what compositors
/// put in the dev_t arrays.
pub fn makedev(major: u32, minor: u32) -> u64 {
    let (ma, mi) = (major as u64, minor as u64);
    ((ma & 0xffff_f000) << 32) | ((ma & 0xfff) << 8) | ((mi & 0xffff_ff00) << 12) | (mi & 0xff)
}

pub fn major_minor(dev: u64) -> (u32, u32) {
    let ma = ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff);
    let mi = ((dev >> 12) & 0xffff_ff00) | (dev & 0xff);
    (ma as u32, mi as u32)
}

/// Value translation, guest side only: the guest kernel knows both clocks and
/// both device numberings.
#[derive(Clone, Default)]
pub struct Rewrites {
    pub devmap: Vec<DevPair>,
    /// host CLOCK_MONOTONIC minus guest CLOCK_MONOTONIC, in ns; shared so the
    /// daemon can refresh it as the kernel's estimate slews.
    pub clock_offset: Arc<AtomicI64>,
}

pub struct EngineConfig {
    pub side: Side,
    pub local: Local,
    pub policy: Policy,
    pub rewrites: Option<Rewrites>,
    /// Send `wp_drm_lease_device_v1.released` to a local client when the
    /// compositor destroyed the device without it.
    pub synth_released: bool,
}

/// The descriptor kinds that need more than a syscall: the guest kernel
/// (resolving a guest dma-buf, adopting a DRM file) or the backend's handle
/// table (PRIME export on the owner's render file, classifying what the
/// compositor sent). Per call, so the backend can pass one borrowing its
/// locked state.
pub trait Platform {
    /// A dma-buf from the local peer, for the channel.
    fn dmabuf_out(&mut self, fd: OwnedFd) -> DescOut;
    /// A DMABUF desc from the channel (and the descriptor the transport made
    /// for it, if any), for the local peer.
    fn dmabuf_in(&mut self, desc: &Desc, fd: Option<OwnedFd>) -> std::io::Result<OwnedFd>;
    /// A DRM file from the local peer, for the channel.
    fn drm_file_out(&mut self, fd: OwnedFd) -> DescOut;
    /// A DRM_FILE desc from the channel, for the local peer.
    fn drm_file_in(&mut self, desc: &Desc, fd: Option<OwnedFd>) -> std::io::Result<OwnedFd>;
    /// A syncobj from the local peer (a client's
    /// `wp_linux_drm_syncobj_manager_v1.import_timeline`), for the channel.
    /// Reached only with `Policy::fences`. The guest daemon hands the fd to
    /// its kernel, which names the host syncobj behind it; a side that cannot
    /// carries nothing (the default).
    fn syncobj_out(&mut self, _fd: OwnedFd) -> DescOut {
        DescOut::plain(Desc::invalid(frame::DESC_SYNCOBJ))
    }
    /// A SYNCOBJ desc from the channel, for the local peer (the backend: the
    /// host syncobj behind the backend handle in `desc.a`).
    fn syncobj_in(&mut self, _desc: &Desc, _fd: Option<OwnedFd>) -> std::io::Result<OwnedFd> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

/// Counters, for logs and tests.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub msgs_to_channel: u64,
    pub msgs_to_local: u64,
    pub globals_offered: u64,
    pub globals_hidden: u64,
    pub commits: u64,
    pub dmabufs: u64,
    pub drm_files: u64,
    pub devt_rewrites: u64,
    pub time_rewrites: u64,
    pub released_synthesised: u64,
    pub placeholders: u64,
    /// `wp_drm_lease_request_v1.submit` requests let through from the channel.
    pub lease_submits: u64,
}

#[derive(Default)]
struct Registry {
    /// Offered globals: name → (interface, version offered).
    offered: HashMap<u32, (IfaceId, u32)>,
    hidden: HashSet<u32>,
}

/// Where a message's descriptors come from.
enum FdSrc<'a> {
    Local(&'a mut VecDeque<OwnedFd>),
    Channel(&'a mut VecDeque<(Desc, Option<OwnedFd>)>),
}

/// What to do with a message after looking at it.
enum Verdict {
    Forward,
    Drop,
}

pub struct Engine {
    cfg: EngineConfig,
    objects: Objects,
    registry: Registry,
    shm: Shm,
    blobs: Blobs,
    streams: Streams,
    out_channel: VecDeque<Out>,
    /// What `out_channel` will put on the channel, in bytes.
    out_bytes: usize,
    /// Past this many bytes for the channel, `from_local` stops.
    input_limit: Option<usize>,
    wl_bytes: Vec<u8>,
    wl_descs: Vec<DescOut>,
    out_local: LocalOut,
    peer_caps: u32,
    got_hello: bool,
    presentation_clock: u32,
    release_pending: HashSet<u32>,
    released_seen: HashSet<u32>,
    hangup: bool,
    /// Lease submits the channel may still carry (`allow_lease_submits`).
    lease_allowance: Option<usize>,
    pub stats: Stats,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        let host = cfg.side == Side::Host;
        let input_limit = (cfg.local == Local::Client).then_some(CHANNEL_HIGH_WATER);
        Self {
            cfg,
            objects: Objects::new(),
            registry: Registry::default(),
            shm: Shm::default(),
            blobs: Blobs::new(host),
            streams: Streams::new(host),
            out_channel: VecDeque::new(),
            out_bytes: 0,
            input_limit,
            wl_bytes: Vec::new(),
            wl_descs: Vec::new(),
            out_local: LocalOut::default(),
            peer_caps: 0,
            got_hello: false,
            // Until wp_presentation.clock_id says otherwise: what every
            // compositor we proxy uses, and what a commit-timing client that
            // never bound wp_presentation is assuming too.
            presentation_clock: CLOCK_MONOTONIC,
            release_pending: HashSet::new(),
            released_seen: HashSet::new(),
            hangup: false,
            lease_allowance: None,
            stats: Stats::default(),
        }
    }

    /// Queue this side's HELLO; the first record either way.
    pub fn hello(&mut self, caps: u32) {
        let h = Hello {
            version: frame::WL_PROTO_VERSION,
            caps: caps | frame::HELLO_STREAM_WINDOW,
        };
        self.push_unit(Unit {
            rec: record(frame::REC_HELLO, 0, 0, &h.encode()),
            descs: Vec::new(),
        });
    }

    pub fn peer_caps(&self) -> Option<u32> {
        self.got_hello.then_some(self.peer_caps)
    }

    pub fn policy_mut(&mut self) -> &mut Policy {
        &mut self.cfg.policy
    }

    /// Charge shm pools, and the buffers made from them, to `b` as well as to
    /// this connection's own limits (`shm.rs`): the backend gives every
    /// connection of a VM the same one, so the number of connections does not
    /// multiply what a guest can make the host hold. Called again, a further
    /// budget is added (a guest process's, beside the VM's).
    pub fn set_shm_budget(&mut self, b: Arc<ShmBudget>) {
        self.blobs.add_budget(b.clone());
        self.shm.set_shared_budget(b);
    }

    /// Charge what stream sinks hold for their readers to `b` as well: the
    /// backend's queue budget, by guest process. Called again, a further one.
    pub fn set_stream_budget(&mut self, b: Arc<dyn ByteBudget>) {
        self.streams.add_budget(b);
    }

    /// Bytes this engine holds in memory for either side: records made for
    /// the channel, output for the local peer, what stream sinks hold, and
    /// unfinished blobs (memfd pages). What commits and blobs still have to
    /// read is not held yet, and is not counted.
    pub fn held_bytes(&self) -> usize {
        let made: usize = self
            .out_channel
            .iter()
            .map(|o| match o {
                Out::Unit(u) => u.bytes(),
                Out::Shm(_) | Out::Blob(_) => 0,
            })
            .sum();
        made + self.wl_bytes.len()
            + self.out_local.len()
            + self.streams.held()
            + self.blobs.held() as usize
    }

    /// How many `wp_drm_lease_request_v1.submit` requests a frame from the
    /// channel carries, looked at before the frame is let in: the backend
    /// rate-limits them, since the compositor answers every lease of a
    /// desktop monitor with blocking modesets (releasing it, and taking it
    /// back when the lease ends). Nothing is changed, and a frame that does
    /// not decode counts none (`from_channel` refuses it). Costs nothing on a
    /// connection that was never offered a lease device.
    ///
    /// This is an estimate made ahead of the engine, and it is not trusted:
    /// every object the frame creates is followed through the protocol tables
    /// as [`Engine::message`] follows it (a registry from
    /// `wl_display.get_registry`, a device from a bind, a request from
    /// `create_lease_request`, anything else by its signature), and what the
    /// engine then lets through is held to it ([`Engine::allow_lease_submits`]):
    /// a submit the count missed ends the connection rather than reaching the
    /// compositor unthrottled.
    pub fn lease_submits(&self, bytes: &[u8]) -> usize {
        if !self
            .registry
            .offered
            .values()
            .any(|(i, _)| *i == proto::WP_DRM_LEASE_DEVICE_V1)
        {
            return 0;
        }
        let Ok(f) = frame::decode(bytes) else {
            return 0;
        };
        let mut fresh: HashMap<u32, IfaceId> = HashMap::new();
        let mut n = 0;
        for r in f.records().filter(|r| r.ty == frame::REC_WAYLAND) {
            let mut p = r.payload;
            while let Some(h) = peek_header(p) {
                let size = h.size as usize;
                if size < 8 || size > p.len() {
                    break;
                }
                let m = &p[..size];
                p = &p[size..];
                let Some(ifc) = fresh.get(&h.object).copied().or_else(|| {
                    self.objects
                        .get(h.object)
                        .filter(|o| !o.zombie)
                        .map(|o| o.iface)
                }) else {
                    // The engine refuses this message and the frame with it.
                    continue;
                };
                if ifc == proto::WP_DRM_LEASE_REQUEST_V1
                    && h.opcode == op::wp_drm_lease_request_v1::REQ_SUBMIT
                {
                    n += 1;
                }
                let Some(desc) = iface(ifc).messages(Dir::Request).get(h.opcode as usize) else {
                    continue;
                };
                let Ok(args) = wire::parse(desc, m) else {
                    continue;
                };
                for (a, at) in desc.args.iter().zip(args.iter()) {
                    if let Val::NewId {
                        id, iface: name, ..
                    } = at.val
                    {
                        let ni = a.iface.or_else(|| name.and_then(proto::iface_by_name));
                        if let Some(ni) = ni {
                            fresh.insert(id, ni);
                        }
                    }
                }
            }
        }
        n
    }

    /// Hold the next frames from the channel to `n` lease submits between
    /// them, or to none with `Some(0)`; `None` (the default) lets any number
    /// through. The backend sets this to what [`Engine::lease_submits`]
    /// counted and its throttle admitted, before each frame: a submit past it
    /// is fatal.
    pub fn allow_lease_submits(&mut self, n: Option<usize>) {
        self.lease_allowance = n;
    }

    /// The connection is over: let go at once of what only a live connection
    /// needs -- shm pools (whose memfds are what a guest commits host memory
    /// through) and half-received blobs -- rather than when the owner gets
    /// round to dropping the engine, which for the backend is whenever the
    /// guest closes the handle.
    pub fn shed(&mut self) {
        self.shm.clear();
        self.blobs.clear();
        self.streams.clear();
        self.drop_channel_output();
    }

    /// Forget what is queued for the channel, without reading what commits
    /// and blobs still had to read: the connection is ending, and nobody
    /// will take it.
    pub fn drop_channel_output(&mut self) {
        self.out_channel.clear();
        self.out_bytes = 0;
        self.wl_bytes.clear();
        self.wl_descs.clear();
    }

    /// End the connection with `f`: nothing more goes to the channel, a
    /// local client is told why with `wl_display.error` -- queued after
    /// what it already has, so it lands after the last whole message and
    /// never inside one that was partly written -- and the ERROR record for
    /// the far side is returned, for the caller to send if the far side is
    /// to know. What the local peer is owed still has to be flushed.
    pub fn end_with(&mut self, f: &Fatal) -> Unit {
        self.drop_channel_output();
        if self.cfg.local == Local::Client {
            self.out_local.push(&f.display_error(), Vec::new());
        }
        f.record()
    }

    /// The far side has gone (HANGUP received).
    pub fn hung_up(&self) -> bool {
        self.hangup
    }

    pub fn local_out(&mut self) -> &mut LocalOut {
        &mut self.out_local
    }

    pub fn local_is_client(&self) -> bool {
        self.cfg.local == Local::Client
    }

    /// Bytes waiting for the local socket.
    pub fn local_out_len(&self) -> usize {
        self.out_local.len()
    }

    /// Everything queued for the channel, in order, read now.
    pub fn take_units(&mut self) -> VecDeque<Unit> {
        self.take_units_upto(usize::MAX)
    }

    /// What is queued for the channel, in order, until about `max` bytes are
    /// taken (always at least one record, if any is queued). Commits' copies
    /// and blobs are read here, a record at a time, so what is not taken yet
    /// costs nothing but a descriptor.
    pub fn take_units_upto(&mut self, max: usize) -> VecDeque<Unit> {
        self.flush_wayland();
        let mut out = VecDeque::new();
        let mut n = 0;
        while n < max || out.is_empty() {
            let Some(front) = self.out_channel.front_mut() else {
                break;
            };
            let shm = matches!(front, Out::Shm(_));
            let Some(job) = front.job_mut() else {
                // A record ready to go.
                let Some(Out::Unit(u)) = self.out_channel.pop_front() else {
                    unreachable!()
                };
                self.out_bytes -= u.bytes();
                n += u.bytes();
                out.push_back(u);
                continue;
            };
            // A job's next record. It was counted at what it had left to
            // read, and is uncounted by what that goes down by -- not by what
            // the step says it read, which on a short read is less (job.rs).
            let before = job.remaining();
            let step = job.next_unit();
            let after = job.remaining().min(before);
            self.out_bytes -= (before - after) as usize;
            let done = step.is_none() || after == 0;
            if let Some((u, got)) = step {
                if shm {
                    self.shm.sync_bytes += got as u64;
                }
                n += u.bytes();
                out.push_back(u);
            }
            if done {
                // Done, or cut short: whatever it still claimed goes too.
                self.out_channel.pop_front();
                self.out_bytes -= after as usize;
            }
        }
        out
    }

    /// What [`Engine::channel_backlog`] should say, counted afresh from the
    /// queue: for tests and the fuzzer, which hold the two equal.
    #[doc(hidden)]
    pub fn channel_backlog_recount(&self) -> usize {
        self.out_channel.iter().map(Out::bytes).sum::<usize>() + self.wl_bytes.len()
    }

    pub fn has_channel_output(&self) -> bool {
        !self.out_channel.is_empty() || !self.wl_bytes.is_empty()
    }

    /// Bytes queued for the channel, read or still to be read.
    pub fn channel_backlog(&self) -> usize {
        self.out_bytes + self.wl_bytes.len()
    }

    /// Stop taking the local peer's input while more than `limit` bytes are
    /// queued for the channel (`None`: never). An engine facing a client
    /// starts at [`CHANNEL_HIGH_WATER`], one facing a compositor at `None`:
    /// a compositor's output is drained as it comes (its own buffer for us
    /// is small, and full, it drops us).
    pub fn set_input_limit(&mut self, limit: Option<usize>) {
        self.input_limit = limit;
    }

    /// `from_local` would take nothing now: the channel has the limit's
    /// worth queued. The rest of the input waits where it is (the caller's
    /// buffer, then the socket) until the channel takes some.
    pub fn input_blocked(&self) -> bool {
        self.input_limit
            .is_some_and(|l| self.channel_backlog() >= l)
    }

    pub fn objects(&self) -> &Objects {
        &self.objects
    }

    pub fn shm_stats(&self) -> (u64, u64) {
        (self.shm.syncs, self.shm.sync_bytes)
    }

    pub fn blob_stats(&self) -> (u64, u64) {
        (self.blobs.sent, self.blobs.received)
    }

    pub fn stream_stats(&self) -> (u64, u64, u64) {
        (
            self.streams.opened,
            self.streams.bytes_out,
            self.streams.bytes_in,
        )
    }

    pub fn stream_interest(&self) -> Vec<Interest> {
        self.streams.interest()
    }

    /// Descriptors of streams that ended, for the caller to stop watching
    /// before it drops them ([`Streams::take_closed`]).
    pub fn take_closed_streams(&mut self) -> Vec<OwnedFd> {
        self.streams.take_closed()
    }

    pub fn stream_io(&mut self, id: u32, readable: bool, writable: bool) {
        let mut out = Vec::new();
        self.streams.io(id, readable, writable, &mut out);
        for u in out {
            self.push_unit(u);
        }
    }

    fn push_unit(&mut self, u: Unit) {
        self.push_out(Out::Unit(u));
    }

    fn push_out(&mut self, o: Out) {
        self.flush_wayland();
        self.out_bytes += o.bytes();
        self.out_channel.push_back(o);
    }

    fn flush_wayland(&mut self) {
        if self.wl_bytes.is_empty() && self.wl_descs.is_empty() {
            return;
        }
        let descs = std::mem::take(&mut self.wl_descs);
        let rec = record(frame::REC_WAYLAND, 0, descs.len() as u32, &self.wl_bytes);
        self.wl_bytes.clear();
        let u = Unit { rec, descs };
        self.out_bytes += u.bytes();
        self.out_channel.push_back(Out::Unit(u));
    }

    fn push_wayland(&mut self, msg: &[u8], descs: Vec<DescOut>) {
        if self.wl_bytes.len() + msg.len() > WL_REC_BYTES
            || self.wl_descs.len() + descs.len() > WL_REC_DESCS
        {
            self.flush_wayland();
        }
        self.wl_bytes.extend_from_slice(msg);
        self.wl_descs.extend(descs);
        self.stats.msgs_to_channel += 1;
    }

    /// The direction of messages arriving from the local socket.
    fn local_dir(&self) -> Dir {
        match self.cfg.local {
            Local::Client => Dir::Request,
            Local::Server => Dir::Event,
        }
    }

    /// Parse every complete message at the front of `data` (bytes read from
    /// the local socket) with the descriptors received alongside, translate
    /// them, and queue the result for the channel. A partial message is left
    /// in `data` for the next read, and so is everything after the message
    /// that brought the channel's queue to the input limit
    /// ([`Engine::input_blocked`]): the caller calls again once the channel
    /// has taken some, whether or not more was read.
    pub fn from_local(
        &mut self,
        data: &mut Vec<u8>,
        fds: &mut VecDeque<OwnedFd>,
        plat: &mut dyn Platform,
    ) -> Result<(), Fatal> {
        let dir = self.local_dir();
        let mut off = 0;
        let res = loop {
            if self.input_blocked() {
                break Ok(());
            }
            let Some(h) = peek_header(&data[off..]) else {
                break Ok(());
            };
            let size = h.size as usize;
            if !(8..=MAX_MSG).contains(&size) || !size.is_multiple_of(4) {
                break Err(Fatal::new(
                    Blame::Local,
                    1,
                    ERR_INVALID_METHOD,
                    "bad message size",
                ));
            }
            if data.len() - off < size {
                break Ok(());
            }
            let msg = data[off..off + size].to_vec();
            off += size;
            if let Err(e) = self.message(dir, true, msg, &mut FdSrc::Local(fds), plat) {
                break Err(e);
            }
        };
        data.drain(..off);
        res
    }

    /// Process one frame from the channel; output goes to the local socket
    /// (and, for credits, back to the channel). `fds` is what the transport
    /// materialised per descriptor (the guest kernel's installed descriptors;
    /// nothing on the host).
    pub fn from_channel(
        &mut self,
        bytes: &[u8],
        fds: Vec<Option<OwnedFd>>,
        plat: &mut dyn Platform,
    ) -> Result<(), Fatal> {
        let f = frame::decode(bytes).map_err(|e| {
            Fatal::new(
                Blame::Channel,
                1,
                ERR_IMPLEMENTATION,
                format!("bad frame: {e:?}"),
            )
        })?;
        let mut fds = fds.into_iter();
        let mut descs: VecDeque<(Desc, Option<OwnedFd>)> =
            f.descs.iter().map(|d| (*d, fds.next().flatten())).collect();
        let dir = match self.local_dir() {
            Dir::Request => Dir::Event,
            Dir::Event => Dir::Request,
        };
        let chan = |m: &str| Fatal::new(Blame::Channel, 1, ERR_IMPLEMENTATION, m.to_string());
        for r in f.records() {
            match r.ty {
                frame::REC_HELLO => {
                    let h = Hello::decode(r.payload).ok_or_else(|| chan("short HELLO"))?;
                    self.peer_caps = h.caps;
                    self.got_hello = true;
                    self.streams
                        .set_peer_windows(h.caps & frame::HELLO_STREAM_WINDOW != 0);
                    if self.cfg.side == Side::Host {
                        self.cfg.policy.drm_file = h.caps & frame::HELLO_G_DRM_FILE != 0;
                        // Explicit sync needs both ends: fences served here
                        // (the policy this engine was made with) and a guest
                        // kernel that names host syncobjs.
                        if h.caps & frame::HELLO_G_SYNCOBJ == 0 {
                            self.cfg.policy.fences = false;
                        }
                    }
                }
                frame::REC_WAYLAND => {
                    let before = descs.len();
                    let mut p = r.payload;
                    while !p.is_empty() {
                        let h = peek_header(p)
                            .ok_or_else(|| chan("partial message in WAYLAND record"))?;
                        let size = h.size as usize;
                        if !(8..=MAX_MSG).contains(&size)
                            || !size.is_multiple_of(4)
                            || size > p.len()
                        {
                            return Err(chan("bad message size in WAYLAND record"));
                        }
                        self.message(
                            dir,
                            false,
                            p[..size].to_vec(),
                            &mut FdSrc::Channel(&mut descs),
                            plat,
                        )?;
                        p = &p[size..];
                    }
                    if before - descs.len() != r.arg as usize {
                        return Err(chan(
                            "WAYLAND record descriptor count disagrees with its messages",
                        ));
                    }
                    self.blobs.expire();
                }
                frame::REC_STREAM_DATA => {
                    let mut out = Vec::new();
                    self.streams
                        .data(r.id, r.payload, &mut out)
                        .map_err(|e| chan(&format!("stream: {e:?}")))?;
                    for u in out {
                        self.push_unit(u);
                    }
                }
                frame::REC_STREAM_EOF => {
                    let mut out = Vec::new();
                    self.streams.eof(r.id, &mut out);
                    for u in out {
                        self.push_unit(u);
                    }
                }
                frame::REC_STREAM_CREDIT => self.streams.credit(r.id, r.arg),
                frame::REC_SHM_SYNC => {
                    if self.cfg.local != Local::Server {
                        return Err(chan("SHM_SYNC toward a client"));
                    }
                    self.shm
                        .sync(r.id, r.arg, r.payload)
                        .map_err(|e| chan(&format!("shm: {e:?}")))?;
                }
                frame::REC_BLOB => {
                    self.blobs
                        .chunk(r.id, r.arg, r.payload)
                        .map_err(|e| chan(&format!("blob: {e:?}")))?;
                }
                frame::REC_ERROR => {
                    return Err(Fatal::new(
                        Blame::Remote,
                        r.id,
                        r.arg,
                        String::from_utf8_lossy(r.payload).into_owned(),
                    ));
                }
                frame::REC_HANGUP => self.hangup = true,
                t => return Err(chan(&format!("unknown record type {t}"))),
            }
        }
        if !descs.is_empty() {
            return Err(chan("descriptors left over after the frame's records"));
        }
        Ok(())
    }

    fn message(
        &mut self,
        dir: Dir,
        from_local: bool,
        mut msg: Vec<u8>,
        src: &mut FdSrc<'_>,
        plat: &mut dyn Platform,
    ) -> Result<(), Fatal> {
        let blame = if from_local {
            Blame::Local
        } else {
            Blame::Channel
        };
        let h = peek_header(&msg).unwrap();
        // Where libwayland-server posts it: the generic errors (an unknown
        // object or opcode, bad arguments, a bad new id, out of memory, the
        // implementation's own) on the display, `wl_display.error`'s object
        // argument being 1 -- the client may not even know the object the
        // message named, and then fails to dispatch the error at all -- and
        // an interface's own errors, and a bad bind, on the object
        // (wayland-server.c, wl_client_connection_data and registry_bind).
        let err = |code: u32, m: String| Fatal::new(blame, 1, code, m);
        let err_on = |code: u32, m: String| Fatal::new(blame, h.object, code, m);

        let obj = self
            .objects
            .get(h.object)
            .ok_or_else(|| err(ERR_INVALID_OBJECT, format!("invalid object {}", h.object)))?;
        let ifc = iface(obj.iface);
        if obj.zombie && dir == Dir::Request {
            return Err(err(
                ERR_INVALID_OBJECT,
                format!("request on destroyed object {}@{}", ifc.name, h.object),
            ));
        }
        let desc = ifc.messages(dir).get(h.opcode as usize).ok_or_else(|| {
            err(
                ERR_INVALID_METHOD,
                format!("{} has no {:?} opcode {}", ifc.name, dir, h.opcode),
            )
        })?;
        if desc.since > obj.version {
            return Err(err(
                ERR_INVALID_METHOD,
                format!(
                    "{}.{} needs version {}, object has {}",
                    ifc.name, desc.name, desc.since, obj.version
                ),
            ));
        }
        let args = wire::parse(desc, &msg).map_err(|e| {
            err(
                ERR_INVALID_METHOD,
                format!("{}.{}: {e:?}", ifc.name, desc.name),
            )
        })?;

        // Registry policy, on whichever side sees the message.
        let mut edits: Vec<(usize, u32)> = Vec::new();
        let mut verdict = Verdict::Forward;
        let mut bind: Option<(IfaceId, u32)> = None;
        if obj.iface == proto::WL_REGISTRY {
            match (dir, h.opcode) {
                (Dir::Event, op::wl_registry::EVT_GLOBAL) => {
                    let (Val::Uint(name), Val::Str(Some(ifname)), Val::Uint(ver)) =
                        (args[0].val, args[1].val, args[2].val)
                    else {
                        unreachable!()
                    };
                    match self.cfg.policy.offer(name, ifname, ver) {
                        Some((id, v)) => {
                            edits.push((args[2].off, v));
                            self.registry.offered.insert(name, (id, v));
                            self.registry.hidden.remove(&name);
                            self.stats.globals_offered += 1;
                        }
                        None => {
                            self.registry.hidden.insert(name);
                            self.stats.globals_hidden += 1;
                            verdict = Verdict::Drop;
                        }
                    }
                }
                (Dir::Event, op::wl_registry::EVT_GLOBAL_REMOVE) => {
                    let Val::Uint(name) = args[0].val else {
                        unreachable!()
                    };
                    if self.registry.hidden.contains(&name) {
                        verdict = Verdict::Drop;
                    }
                }
                (Dir::Request, op::wl_registry::REQ_BIND) => {
                    let (
                        Val::Uint(name),
                        Val::NewId {
                            iface: Some(ifname),
                            version,
                            ..
                        },
                    ) = (args[0].val, args[1].val)
                    else {
                        unreachable!()
                    };
                    let Some(&(id, max)) = self.registry.offered.get(&name) else {
                        return Err(err_on(
                            ERR_INVALID_OBJECT,
                            format!("bind of global {name}, which was not offered"),
                        ));
                    };
                    if iface(id).name.as_bytes() != ifname || version == 0 || version > max {
                        return Err(err_on(
                            ERR_INVALID_OBJECT,
                            format!(
                                "bind of global {name} as {} v{version}; offered {} v{max}",
                                String::from_utf8_lossy(ifname),
                                iface(id).name
                            ),
                        ));
                    }
                    bind = Some((id, version));
                }
                _ => {}
            }
        }
        if matches!(verdict, Verdict::Drop) {
            // A hidden global's event carries no descriptors or new objects.
            return Ok(());
        }

        // New objects. Their side state (shm) is cleared first: the id may be
        // a reused one.
        for (a, at) in desc.args.iter().zip(args.iter()) {
            if let Val::NewId { id, .. } = at.val {
                let (ni, nv) = match (a.iface, bind) {
                    (Some(i), _) => (i, obj.version),
                    (None, Some(b)) => b,
                    (None, None) => {
                        return Err(err(
                            ERR_INVALID_METHOD,
                            "untyped new_id outside bind".into(),
                        ));
                    }
                };
                self.objects
                    .create(id, ni, nv, dir == Dir::Request)
                    .map_err(|e| {
                        let m = match e {
                            ObjError::InUse(i) => format!("new id {i} is in use"),
                            ObjError::WrongRange(i) => {
                                format!("new id {i} is in the other side's range")
                            }
                            ObjError::TooMany => "too many objects".to_string(),
                        };
                        err(
                            if e == ObjError::TooMany {
                                ERR_NO_MEMORY
                            } else {
                                ERR_INVALID_OBJECT
                            },
                            m,
                        )
                    })?;
                self.shm.forget(id);
            }
        }

        // Guest-side value rewrites.
        if let (Some(rw), Some(kind)) = (&self.cfg.rewrites, desc.rewrite) {
            let rw = rw.clone();
            self.rewrite(&rw, kind, &args, from_local, &mut edits);
        }

        // Descriptors, by class.
        let uint = |i: u8| match args[i as usize].val {
            Val::Uint(v) => v,
            _ => 0,
        };
        let new_id_at = |i: usize| match args.get(i).map(|a| a.val) {
            Some(Val::NewId { id, .. }) => id,
            _ => 0,
        };
        let mut out_descs: Vec<DescOut> = Vec::new();
        let mut out_fds: Vec<OwnedFd> = Vec::new();
        let mut pre_units: Vec<Unit> = Vec::new();
        let mut pre_job: Option<Out> = None;
        if desc.nfds > 0 {
            let class = desc.fd.ok_or_else(|| {
                err(
                    ERR_IMPLEMENTATION,
                    format!(
                        "{}.{} carries a descriptor the proxy cannot handle",
                        ifc.name, desc.name
                    ),
                )
            })?;
            match src {
                FdSrc::Local(q) => {
                    let fd = q.pop_front().ok_or_else(|| {
                        err(
                            ERR_INVALID_METHOD,
                            format!("{}.{}: descriptor expected", ifc.name, desc.name),
                        )
                    })?;
                    let d = match class {
                        FdKind::ShmPool => {
                            let size = match args[2].val {
                                Val::Int(s) if s > 0 => s as u64,
                                _ => {
                                    return Err(err_on(
                                        ERR_SHM_INVALID_STRIDE,
                                        "invalid shm pool size".into(),
                                    ));
                                }
                            };
                            // Read at every commit, on the thread that
                            // serves the rest of the connection: memory,
                            // not a file whose server decides how long a
                            // read takes (sys::is_shmem).
                            if !sys::is_shmem(fd.as_raw_fd()) {
                                return Err(err_on(
                                    ERR_SHM_INVALID_FD,
                                    "an shm pool must be a memfd or a file on tmpfs".into(),
                                ));
                            }
                            // The client's own memory, but a descriptor
                            // held here: its count, and what its buffers
                            // cover, are charged.
                            let charge = self
                                .shm
                                .charge()
                                .map_err(|_| err(ERR_NO_MEMORY, "too many shm pools".into()))?;
                            self.shm.add_pool(new_id_at(0), fd, size, charge, false);
                            DescOut::plain(Desc {
                                c: size,
                                ..Desc::new(frame::DESC_SHM_POOL)
                            })
                        }
                        FdKind::Dmabuf => {
                            self.stats.dmabufs += 1;
                            plat.dmabuf_out(fd)
                        }
                        FdKind::Blob {
                            size_arg,
                            offset_arg,
                        } => {
                            let len = uint(size_arg) as u64;
                            let off = offset_arg.map(|o| uint(o) as u64).unwrap_or(0);
                            if let Some(o) = offset_arg {
                                edits.push((args[o as usize].off, 0));
                            }
                            // A client's file is read as the channel takes
                            // it, as a pool is: memory only. A compositor's
                            // (a keymap) is trusted to be readable.
                            let readable =
                                self.cfg.local == Local::Server || sys::is_shmem(fd.as_raw_fd());
                            let sent = if readable {
                                self.blobs.send(fd, off, len, &mut pre_units)
                            } else {
                                None
                            };
                            match sent {
                                Some((id, job)) => {
                                    pre_job = job.map(Out::Blob);
                                    DescOut::plain(Desc {
                                        a: id,
                                        c: len,
                                        ..Desc::new(frame::DESC_BLOB)
                                    })
                                }
                                None => DescOut::plain(Desc::invalid(frame::DESC_BLOB)),
                            }
                        }
                        FdKind::Stream => {
                            if !sys::is_fifo(fd.as_raw_fd()) {
                                return Err(err(
                                    ERR_INVALID_METHOD,
                                    format!("{}.{}: not a pipe", ifc.name, desc.name),
                                ));
                            }
                            match self.streams.add_sink(fd) {
                                Ok((id, first)) => DescOut::plain(Desc {
                                    a: id,
                                    b: first,
                                    ..Desc::new(frame::DESC_STREAM)
                                }),
                                Err(_) => DescOut::plain(Desc::invalid(frame::DESC_STREAM)),
                            }
                        }
                        FdKind::DrmFile => {
                            self.stats.drm_files += 1;
                            plat.drm_file_out(fd)
                        }
                        // The syncobj global is only offered with fences
                        // (policy_table.rs), so without them no object can
                        // carry one here and this is a peer out of step.
                        FdKind::Syncobj if self.cfg.policy.fences => plat.syncobj_out(fd),
                        FdKind::Syncobj => {
                            return Err(err(
                                ERR_IMPLEMENTATION,
                                "syncobj descriptors are not bridged".into(),
                            ));
                        }
                    };
                    out_descs.push(d);
                }
                FdSrc::Channel(q) => {
                    let (d, tfd) = q.pop_front().ok_or_else(|| {
                        err(
                            ERR_IMPLEMENTATION,
                            format!("{}.{}: no descriptor in the frame", ifc.name, desc.name),
                        )
                    })?;
                    let want = match class {
                        FdKind::ShmPool => frame::DESC_SHM_POOL,
                        FdKind::Dmabuf => frame::DESC_DMABUF,
                        FdKind::Blob { .. } => frame::DESC_BLOB,
                        FdKind::Stream => frame::DESC_STREAM,
                        FdKind::DrmFile => frame::DESC_DRM_FILE,
                        FdKind::Syncobj => frame::DESC_SYNCOBJ,
                    };
                    if d.kind != want {
                        return Err(err(
                            ERR_IMPLEMENTATION,
                            format!(
                                "{}.{}: descriptor of kind {} where {want} belongs",
                                ifc.name, desc.name, d.kind
                            ),
                        ));
                    }
                    let fd = if d.is_invalid() {
                        None
                    } else {
                        match class {
                            FdKind::ShmPool => {
                                let size = match args[2].val {
                                    Val::Int(s) if s > 0 && s as u64 == d.c => d.c,
                                    _ => {
                                        return Err(err(
                                            ERR_IMPLEMENTATION,
                                            "shm pool size disagrees".into(),
                                        ));
                                    }
                                };
                                // Counted before the memfd exists: past this
                                // connection's or the VM's pool count, no
                                // memfd. Its size costs nothing until a
                                // buffer made from it is charged.
                                let charge = self.shm.charge().map_err(|_| {
                                    err(
                                        ERR_NO_MEMORY,
                                        "too many shm pools on the connection or the VM".into(),
                                    )
                                })?;
                                let memfd = sys::memfd(c"nvgpu-wl-shm", size)
                                    .map_err(|e| err(ERR_NO_MEMORY, format!("shm pool: {e}")))?;
                                let give = memfd
                                    .try_clone()
                                    .map_err(|e| err(ERR_NO_MEMORY, format!("dup: {e}")))?;
                                self.shm.add_pool(new_id_at(0), memfd, size, charge, true);
                                Some(give)
                            }
                            FdKind::Dmabuf => {
                                self.stats.dmabufs += 1;
                                plat.dmabuf_in(&d, tfd).ok()
                            }
                            FdKind::Blob { size_arg, .. } => {
                                if d.c != uint(size_arg) as u64 {
                                    return Err(err(
                                        ERR_IMPLEMENTATION,
                                        "blob size disagrees with message".into(),
                                    ));
                                }
                                Some(
                                    self.blobs.take(d.a, d.c).map_err(|e| {
                                        err(ERR_IMPLEMENTATION, format!("blob: {e:?}"))
                                    })?,
                                )
                            }
                            FdKind::Stream => {
                                Some(self.streams.add_source(d.a, d.b).map_err(|e| {
                                    err(ERR_IMPLEMENTATION, format!("stream: {e:?}"))
                                })?)
                            }
                            FdKind::DrmFile => {
                                self.stats.drm_files += 1;
                                plat.drm_file_in(&d, tfd).ok()
                            }
                            FdKind::Syncobj => plat.syncobj_in(&d, tfd).ok(),
                        }
                    };
                    let fd = match fd {
                        Some(f) => f,
                        // A timeline nobody can name (the guest kernel found
                        // no host syncobj behind the client's file: another
                        // device's, or no syncobj at all) ends the client
                        // here, with the error the protocol has for it. A
                        // placeholder would earn it the same error from the
                        // compositor (Hyprland's CSyncTimeline::create fails
                        // on it, DRMSyncobj.cpp:128-131), only later, from a
                        // host process, and naming nothing -- unlike a dma-buf,
                        // whose placeholder only fails that one buffer.
                        None if class == FdKind::Syncobj => {
                            return Err(err_on(
                                ERR_SYNCOBJ_INVALID_TIMELINE,
                                "import_timeline: the syncobj is not one of the virtio-nvgpu \
                                 device's (a timeline from another DRM device cannot reach \
                                 the host compositor)"
                                    .into(),
                            ));
                        }
                        None => {
                            self.stats.placeholders += 1;
                            sys::placeholder_fd()
                                .map_err(|e| err(ERR_NO_MEMORY, format!("placeholder: {e}")))?
                        }
                    };
                    out_fds.push(fd);
                }
            }
        }

        // shm bookkeeping, and the copy a commit needs, on the client's side;
        // the pool geometry on both.
        let client_side = self.cfg.local == Local::Client;
        let server_side = !client_side;
        let obj_id = h.object;
        let mut commit_sync: Option<SyncJob> = None;
        if dir == Dir::Request {
            match (obj.iface, h.opcode) {
                (proto::WL_SHM_POOL, op::wl_shm_pool::REQ_CREATE_BUFFER) => {
                    let g = |i: usize| match args[i].val {
                        Val::Int(v) => v,
                        _ => 0,
                    };
                    self.shm
                        .add_buffer(obj_id, new_id_at(0), g(1), g(3), g(4))
                        .map_err(|_| {
                            err(
                                ERR_NO_MEMORY,
                                "shm buffer over the connection's or the VM's budget".into(),
                            )
                        })?;
                }
                (proto::WL_SHM_POOL, op::wl_shm_pool::REQ_RESIZE) => {
                    if let Val::Int(s) = args[0].val
                        && s > 0
                    {
                        self.shm
                            .resize(obj_id, s as u64, server_side)
                            .map_err(|e| err(ERR_NO_MEMORY, format!("shm resize: {e:?}")))?;
                    }
                }
                (proto::WL_SURFACE, op::wl_surface::REQ_ATTACH) if client_side => {
                    if let Val::Object(b) = args[0].val {
                        self.shm.attach(obj_id, b);
                    }
                }
                (proto::WL_SURFACE, op::wl_surface::REQ_DAMAGE) if client_side => {
                    self.shm.damage_surface(obj_id)
                }
                (proto::WL_SURFACE, op::wl_surface::REQ_DAMAGE_BUFFER) if client_side => {
                    if let (Val::Int(y), Val::Int(hh)) = (args[1].val, args[3].val) {
                        self.shm.damage_buffer(obj_id, y, hh);
                    }
                }
                (proto::WL_SURFACE, op::wl_surface::REQ_COMMIT) => {
                    self.stats.commits += 1;
                    if client_side {
                        commit_sync = self.shm.commit(obj_id);
                    }
                }
                (proto::WP_DRM_LEASE_DEVICE_V1, op::wp_drm_lease_device_v1::REQ_RELEASE) => {
                    self.release_pending.insert(obj_id);
                }
                (proto::WP_DRM_LEASE_REQUEST_V1, op::wp_drm_lease_request_v1::REQ_SUBMIT)
                    if !from_local =>
                {
                    match &mut self.lease_allowance {
                        None => {}
                        Some(0) => {
                            return Err(err(
                                ERR_IMPLEMENTATION,
                                "a lease submit the rate limit did not admit".into(),
                            ));
                        }
                        Some(k) => *k -= 1,
                    }
                    self.stats.lease_submits += 1;
                }
                _ => {}
            }
        }

        // A lease device destroyed without `released`: supply it before the
        // delete_id that frees the id.
        let mut synth: Option<Vec<u8>> = None;
        if dir == Dir::Event
            && obj.iface == proto::WL_DISPLAY
            && h.opcode == op::wl_display::EVT_DELETE_ID
            && let Val::Uint(id) = args[0].val
            && self.cfg.synth_released
            && !from_local
            && self.release_pending.remove(&id)
            && !self.released_seen.remove(&id)
            && self
                .objects
                .get(id)
                .is_some_and(|o| !o.zombie && o.iface == proto::WP_DRM_LEASE_DEVICE_V1)
        {
            synth = Some(MsgBuilder::new(id, op::wp_drm_lease_device_v1::EVT_RELEASED).finish());
        }
        if dir == Dir::Event
            && obj.iface == proto::WP_DRM_LEASE_DEVICE_V1
            && h.opcode == op::wp_drm_lease_device_v1::EVT_RELEASED
        {
            self.release_pending.remove(&obj_id);
            self.released_seen.insert(obj_id);
        }
        if let Some(s) = synth {
            self.stats.released_synthesised += 1;
            self.message(
                Dir::Event,
                false,
                s,
                &mut FdSrc::Channel(&mut VecDeque::new()),
                plat,
            )?;
        }

        // Lifetimes.
        if desc.destructor {
            match dir {
                Dir::Request => self.objects.destroyed_by_request(obj_id),
                Dir::Event => self.objects.destroyed_by_event(obj_id),
            }
            self.shm.forget(obj_id);
        }
        if dir == Dir::Event
            && obj.iface == proto::WL_DISPLAY
            && h.opcode == op::wl_display::EVT_DELETE_ID
            && let Val::Uint(id) = args[0].val
        {
            self.objects.delete_id(id);
            self.shm.forget(id);
            self.release_pending.remove(&id);
            self.released_seen.remove(&id);
        }

        drop(args);
        for (off, v) in edits {
            put_word(&mut msg, off, v);
        }

        if from_local {
            for u in pre_units {
                self.push_unit(u);
            }
            if let Some(j) = pre_job {
                self.push_out(j);
            }
            if let Some(j) = commit_sync {
                self.push_out(Out::Shm(j));
            }
            self.push_wayland(&msg, out_descs);
        } else {
            self.out_local.push(&msg, out_fds);
            self.stats.msgs_to_local += 1;
        }
        Ok(())
    }

    fn rewrite(
        &mut self,
        rw: &Rewrites,
        kind: RewriteKind,
        args: &[At<'_>],
        from_local: bool,
        edits: &mut Vec<(usize, u32)>,
    ) {
        // From the channel is host → guest; from the local peer, guest → host.
        let to_guest = !from_local;
        match kind {
            RewriteKind::ClockId(i) => {
                if let Val::Uint(c) = args[i as usize].val {
                    self.presentation_clock = c;
                }
            }
            RewriteKind::DevT(i) => {
                let at = &args[i as usize];
                if let Val::Array(a) = at.val
                    && a.len() == 8
                {
                    let dev = u64::from_ne_bytes(a.try_into().unwrap());
                    let mm = major_minor(dev);
                    let mapped = rw
                        .devmap
                        .iter()
                        .find(|p| {
                            if to_guest {
                                p.host == mm
                            } else {
                                p.guest == mm
                            }
                        })
                        .map(|p| if to_guest { p.guest } else { p.host });
                    // A node with no counterpart (another GPU of the host's,
                    // say) must not alias one of ours: 0 says "unknown
                    // device" rather than naming the wrong one.
                    let new = mapped.map(|(ma, mi)| makedev(ma, mi)).unwrap_or(0);
                    let b = new.to_ne_bytes();
                    edits.push((at.off + 4, u32::from_ne_bytes(b[0..4].try_into().unwrap())));
                    edits.push((at.off + 8, u32::from_ne_bytes(b[4..8].try_into().unwrap())));
                    self.stats.devt_rewrites += 1;
                }
            }
            RewriteKind::Timestamp {
                sec_hi,
                sec_lo,
                nsec,
            } => {
                if self.presentation_clock != CLOCK_MONOTONIC
                    && self.presentation_clock != CLOCK_MONOTONIC_RAW
                {
                    return;
                }
                let u = |i: u8| match args[i as usize].val {
                    Val::Uint(v) => v as u64,
                    _ => 0,
                };
                let t = ((u(sec_hi) << 32 | u(sec_lo)) as i128) * 1_000_000_000 + u(nsec) as i128;
                if t == 0 {
                    return;
                }
                let off = rw.clock_offset.load(Ordering::Relaxed) as i128;
                let t = if to_guest { t - off } else { t + off }.max(0);
                let secs = (t / 1_000_000_000) as u64;
                let ns = (t % 1_000_000_000) as u32;
                edits.push((args[sec_hi as usize].off, (secs >> 32) as u32));
                edits.push((args[sec_lo as usize].off, secs as u32));
                edits.push((args[nsec as usize].off, ns));
                self.stats.time_rewrites += 1;
            }
        }
    }

    /// Whether `argkind` exists in the signature (used by tests).
    pub fn signature_has(iface_id: IfaceId, dir: Dir, opcode: u16, kind: ArgKind) -> bool {
        iface(iface_id)
            .messages(dir)
            .get(opcode as usize)
            .is_some_and(|m| m.args.iter().any(|a| a.kind == kind))
    }
}
