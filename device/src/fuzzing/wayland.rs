// SPDX-License-Identifier: Apache-2.0
//! The Wayland engine (`wlwire`), both ends, both directions.
//!
//! A guest engine facing an app and a host engine facing the compositor,
//! joined by frames as the channel joins them. The input is a sequence of
//! operations, each one of:
//!
//! - a message from the app to the guest daemon, or from the compositor to
//!   the backend, either as raw bytes or built from the protocol tables
//!   against an object that end knows (so the fuzzer gets past the registry
//!   and binds without guessing ids), with descriptors of every class the
//!   policy knows (shm pools, pipes, dma-buf and DRM-file stand-ins) attached;
//! - a frame from the channel, raw, or built from records of any type
//!   (Wayland, stream data, EOF, credit, SHM_SYNC, blob, error, hangup) and
//!   descriptors of any kind, into either end -- the guest's frames into the
//!   host engine are the boundary that matters most;
//! - moving what each end has queued across, as the channel would;
//! - stream readiness;
//! - time passing, for the lease throttle.
//!
//! The configuration byte picks normal mode (the guest faces an app, the host
//! a compositor) or export mode (the guest faces a compositor, the host a
//! host client), fences, DRM files, the lease device, and the lease rate.
//! Every frame into the host engine goes as `WlConn::send` sends it: its
//! lease submits counted (`Engine::lease_submits`), admitted by a
//! `LeaseThrottle` on a clock the input moves, and held to that count.
//!
//! A `Fatal` from an end is that connection closed, and ends the run, as it
//! would the connection. Anything else wrong -- a panic, a descriptor still
//! open after both ends are dropped -- is the finding, and so is:
//!
//! - an engine holding more memory than its budgets allow
//!   (`Engine::held_bytes`, after every operation);
//! - a descriptor open between operations that no engine counts
//!   (`Engine::held_fds`);
//! - an engine whose count of what it has queued for the channel is not
//!   what its queue holds, or that still counts something once it has
//!   nothing more to take (the input limit is decided by that count);
//! - more lease submits reaching the compositor than the throttle admitted,
//!   or the throttle admitting more than its burst and rate allow, or still
//!   refusing once the wait it named has passed.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wlwire::engine::{DevPair, Engine, EngineConfig, Local, Platform, Rewrites, Side};
use wlwire::frame::{self, Desc, DescOut, Unit};
use wlwire::localin::LocalIn;
use wlwire::policy::{LeaseGate, Policy};
use wlwire::proto::{self, ArgKind, Dir};
use wlwire::shm::ShmBudget;
use wlwire::stream::ByteBudget;
use wlwire::wire::{MsgBuilder, SERVER_ID_START};

use super::Bytes;
use crate::wl::{LeaseRefusal, LeaseThrottle};

/// What the host engine may hold: its process's shm budget (unfinished
/// blobs), its stream budget, and what one operation's input can make
/// before the next pump (at most 128 operations of at most 1 MiB each, and
/// much less in practice).
const HOST_HELD: usize = (16 << 20) + STREAM_BUDGET + (32 << 20);
/// The guest engine has no shared budgets: unfinished blobs to their
/// per-connection limit, stream sinks to theirs, and the same slack.
const GUEST_HELD: usize = (64 << 20) + (16 << 20) + (32 << 20);
/// The host's stream sinks, as the backend's queue budget share would be.
const STREAM_BUDGET: usize = 8 << 20;

/// A byte budget for the host's stream sinks.
struct Budget(Mutex<usize>, usize);

impl Budget {
    fn used(&self) -> usize {
        *self.0.lock().unwrap()
    }
}

impl ByteBudget for Budget {
    fn take(&self, n: usize) -> bool {
        let mut u = self.0.lock().unwrap();
        match u.checked_add(n).filter(|&t| t <= self.1) {
            Some(t) => {
                *u = t;
                true
            }
            None => false,
        }
    }
    fn give(&self, n: usize) {
        *self.0.lock().unwrap() -= n;
    }
}

struct Plat;

fn memfd(size: u64) -> OwnedFd {
    wlwire::sys::memfd(c"fuzz", size).expect("memfd")
}

impl Platform for Plat {
    fn dmabuf_out(&mut self, fd: OwnedFd) -> DescOut {
        DescOut::plain(Desc {
            a: 7,
            b: fd.as_raw_fd() as u32,
            ..Desc::new(frame::DESC_DMABUF)
        })
    }
    fn dmabuf_in(&mut self, d: &Desc, _fd: Option<OwnedFd>) -> std::io::Result<OwnedFd> {
        if d.a == 0 {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        Ok(memfd(4096))
    }
    fn drm_file_out(&mut self, fd: OwnedFd) -> DescOut {
        DescOut {
            desc: Desc {
                b: 4,
                ..Desc::new(frame::DESC_DRM_FILE)
            },
            fd: Some(fd),
        }
    }
    fn drm_file_in(&mut self, _d: &Desc, fd: Option<OwnedFd>) -> std::io::Result<OwnedFd> {
        fd.ok_or_else(|| std::io::Error::other("no fd"))
    }
    fn syncobj_out(&mut self, _fd: OwnedFd) -> DescOut {
        DescOut::plain(Desc {
            a: 3,
            ..Desc::new(frame::DESC_SYNCOBJ)
        })
    }
    fn syncobj_in(&mut self, d: &Desc, _fd: Option<OwnedFd>) -> std::io::Result<OwnedFd> {
        if d.a == 0 {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        crate::sys::fd::eventfd(libc::EFD_CLOEXEC)
    }
}

/// A descriptor of the kind `k` names, as an app or compositor would pass.
fn some_fd(k: u8, size: u64) -> OwnedFd {
    match k % 5 {
        0 | 1 => {
            let fd = memfd(size % (4 << 20));
            let _ = wlwire::sys::ftruncate(fd.as_raw_fd(), size % (4 << 20));
            fd
        }
        2 => {
            let (r, w) = wlwire::sys::pipe().expect("pipe");
            if k & 0x80 != 0 { r } else { w }
        }
        3 => wlwire::sys::eventfd().expect("eventfd"),
        _ => std::fs::File::open("/dev/null").expect("/dev/null").into(),
    }
}

struct Pair {
    g: Engine,
    h: Engine,
    /// The last client id the app used, and server id the compositor did.
    ids: (u32, u32),
    /// Export mode: the guest faces a compositor, the host a client.
    export: bool,
    lease: LeaseThrottle,
    interval: Duration,
    burst: u32,
    /// The throttle's clock: a fixed start, and how far the input moved it.
    t0: Instant,
    elapsed: Duration,
    /// Submits the throttle admitted.
    admitted: u64,
    streams: Arc<Budget>,
}

fn engines(cfg: u8, rate: u8) -> Pair {
    let export = cfg & 0x10 != 0;
    let interval = Duration::from_millis(250 * u64::from(rate % 8 + 1));
    let burst = u32::from(rate >> 3) % 4 + 1;
    let rewrites = Rewrites {
        devmap: vec![DevPair {
            host: (226, 129),
            guest: (226, 128),
        }],
        clock_offset: Arc::new(AtomicI64::new(i64::from(cfg as i8) * 1_000_000)),
    };
    // Normal mode as the daemon and WlConn::open make them; export mode as
    // the daemon's export side and WlConn::from_export do.
    let mut g = Engine::new(EngineConfig {
        side: Side::Guest,
        local: if export { Local::Server } else { Local::Client },
        policy: Policy {
            drm_file: !export,
            lease: LeaseGate::Allow,
            fences: !export && cfg & 2 != 0,
        },
        rewrites: Some(rewrites),
        synth_released: !export,
    });
    g.set_input_limit(Some(wlwire::engine::CHANNEL_HIGH_WATER));
    let mut h = Engine::new(EngineConfig {
        side: Side::Host,
        local: if export { Local::Client } else { Local::Server },
        policy: if export {
            Policy::default()
        } else {
            Policy {
                drm_file: cfg & 4 != 0,
                lease: if cfg & 8 != 0 {
                    LeaseGate::Allow
                } else {
                    LeaseGate::Deny
                },
                fences: cfg & 2 != 0,
            }
        },
        rewrites: None,
        synth_released: false,
    });
    // The VM's budget and a process's, as the backend gives every connection.
    h.set_shm_budget(Arc::new(ShmBudget::new(64 << 20, 64)));
    h.set_shm_budget(Arc::new(ShmBudget::new(16 << 20, 16)));
    let streams = Arc::new(Budget(Mutex::new(0), STREAM_BUDGET));
    h.set_stream_budget(streams.clone());
    g.hello(if export {
        frame::HELLO_G_DMABUF_IMPORT
    } else {
        frame::HELLO_G_DRM_FILE
            | if cfg & 2 != 0 {
                frame::HELLO_G_SYNCOBJ
            } else {
                0
            }
    });
    h.hello(0);
    Pair {
        g,
        h,
        ids: (2, SERVER_ID_START),
        export,
        lease: LeaseThrottle::new(interval, burst),
        interval,
        burst,
        t0: Instant::now(),
        elapsed: Duration::ZERO,
        admitted: 0,
        streams,
    }
}

/// Build one message from the tables: an object `e` knows, one of its
/// messages in direction `dir`, arguments from the input.
fn build(
    b: &mut Bytes,
    objects: &wlwire::objects::Objects,
    dir: Dir,
    ids: &mut (u32, u32),
    fds: &mut VecDeque<OwnedFd>,
) -> Option<Vec<u8>> {
    // An object this end knows: display, registry and whatever was bound.
    let id = match b.u8() {
        0 => 1,
        n if n >= 0xf0 => SERVER_ID_START + u32::from(n & 0xf),
        n => u32::from(n % 64) + 1,
    };
    let obj = objects.get(id)?;
    let msgs = proto::iface(obj.iface).messages(dir);
    if msgs.is_empty() {
        return None;
    }
    let opcode = b.u8() as usize % msgs.len();
    let m = &msgs[opcode];
    let mut mb = MsgBuilder::new(id, opcode as u16);
    for a in m.args {
        mb = match a.kind {
            ArgKind::Int => mb.int(b.u32() as i32),
            ArgKind::Uint => mb.uint(match b.u8() {
                0 => 0,
                1 => u32::MAX,
                n if n & 1 == 1 => b.u32(),
                n => u32::from(n),
            }),
            ArgKind::Fixed => mb.fixed(f64::from(b.u16() as i16) / 4.0),
            ArgKind::Str => {
                let pick = b.u8();
                if pick & 1 == 0 {
                    let i = pick as usize / 2 % proto::INTERFACES.len();
                    mb.string(Some(proto::INTERFACES[i].name))
                } else if pick == 0xff {
                    mb.string(None)
                } else {
                    let n = b.u8() as usize % 64;
                    let s: String = b.take(n).iter().map(|&c| (c % 94 + 32) as char).collect();
                    mb.string(Some(&s))
                }
            }
            ArgKind::Object => mb.object(match b.u8() {
                0 => 0,
                n if n >= 0xf0 => SERVER_ID_START + u32::from(n & 0xf),
                n => u32::from(n % 64) + 1,
            }),
            ArgKind::NewId if a.iface.is_some() => {
                let id = if dir == Dir::Request {
                    ids.0 += 1;
                    ids.0
                } else {
                    ids.1 += 1;
                    ids.1
                };
                mb.new_id(id)
            }
            ArgKind::NewId => {
                ids.0 += 1;
                let i = b.u8() as usize % proto::INTERFACES.len();
                let f = &proto::INTERFACES[i];
                mb.generic_new_id(f.name, u32::from(b.u8() % 8).max(1).min(f.version), ids.0)
            }
            ArgKind::Array => {
                let n = b.u8() as usize % 64;
                mb.array(b.take(n))
            }
            ArgKind::Fd => {
                fds.push_back(some_fd(b.u8(), u64::from(b.u32())));
                mb
            }
        };
    }
    Some(mb.finish())
}

/// A frame built from records and descriptors of the input's choosing.
fn record_frame(b: &mut Bytes) -> (Vec<u8>, Vec<Option<OwnedFd>>) {
    let mut q = VecDeque::new();
    for _ in 0..(b.u8() % 4 + 1) {
        let ty = u16::from(b.u8() % 10);
        // Ids of either side's half (streams and blobs are the sender's),
        // payloads up to a record's most.
        let half = if b.u8() & 1 != 0 { 0x8000_0000 } else { 0 };
        let (id, arg) = (b.u32() % 64 | half, b.u32());
        let n = b.u16() as usize;
        let payload = b.take(n).to_vec();
        let descs = (0..b.u8() % 4)
            .map(|_| {
                DescOut::plain(Desc {
                    kind: u16::from(b.u8() % 8),
                    flags: u16::from(b.u8() & 1),
                    fd: -1,
                    a: b.u32(),
                    b: b.u32(),
                    c: b.u64() % (1 << 24),
                })
            })
            .collect();
        q.push_back(Unit {
            rec: frame::record(ty, id, arg, &payload),
            descs,
        });
    }
    frame::pack(&mut q, 1 << 20, 256, b.u8() & 1 != 0)
}

impl Pair {
    /// Directions of what each end's local peer sends: (guest's, host's).
    fn local_dirs(&self) -> (Dir, Dir) {
        if self.export {
            (Dir::Event, Dir::Request)
        } else {
            (Dir::Request, Dir::Event)
        }
    }

    /// A frame into the host engine, as `WlConn::send` takes it: its lease
    /// submits admitted first (retried once the throttle's wait has passed,
    /// as the daemon retries on EAGAIN), and the engine held to them.
    fn to_host(&mut self, f: &[u8], fds: Vec<Option<OwnedFd>>) -> bool {
        let n = self.h.lease_submits(f);
        match self.lease.admit(n, self.t0 + self.elapsed) {
            Ok(()) => {}
            Err(LeaseRefusal::OverBurst) => {
                assert!(
                    n > self.burst as usize,
                    "{n} submits refused as over a burst of {}",
                    self.burst
                );
                return false;
            }
            Err(LeaseRefusal::Wait(w)) => {
                assert!(w > Duration::ZERO);
                self.elapsed += w;
                assert_eq!(
                    self.lease.admit(n, self.t0 + self.elapsed),
                    Ok(()),
                    "{n} submits still refused after the wait the throttle named"
                );
            }
        }
        self.admitted += n as u64;
        // GCRA: never more than the burst, plus one per interval since.
        let allowed =
            u64::from(self.burst) + (self.elapsed.as_nanos() / self.interval.as_nanos()) as u64;
        assert!(
            self.admitted <= allowed,
            "{} submits admitted in {:?} at {:?} a burst of {}",
            self.admitted,
            self.elapsed,
            self.interval,
            self.burst
        );
        self.h.allow_lease_submits(Some(n));
        let ok = self.h.from_channel(f, fds, &mut Plat).is_ok();
        self.h.allow_lease_submits(Some(0));
        assert!(
            self.h.stats.lease_submits <= self.admitted,
            "{} submits reached the compositor, {} admitted",
            self.h.stats.lease_submits,
            self.admitted
        );
        ok
    }

    /// Neither engine holds more than its budgets allow, and what each
    /// counts as queued for the channel is what its queue holds.
    fn check_memory(&self) {
        let (g, h) = (self.g.held_bytes(), self.h.held_bytes());
        assert!(h <= HOST_HELD, "the host engine holds {h} bytes");
        assert!(g <= GUEST_HELD, "the guest engine holds {g} bytes");
        assert!(self.streams.used() <= STREAM_BUDGET);
        for (e, side) in [(&self.g, "guest"), (&self.h, "host")] {
            assert_eq!(
                e.channel_backlog(),
                e.channel_backlog_recount(),
                "the {side} engine's backlog disagrees with its queue"
            );
        }
    }

    /// Move everything queued on either side across until quiet (bounded),
    /// a frame at a time as the daemon and the backend take it.
    fn pump(&mut self) -> bool {
        for _ in 0..16 {
            let mut moved = false;
            loop {
                let mut q = self.g.take_units_upto(1 << 20);
                if q.is_empty() {
                    // Nothing left to take is nothing left counted: a count
                    // that outlives its queue blocks the app's input for
                    // good.
                    assert_eq!(self.g.channel_backlog(), 0, "a backlog with nothing queued");
                    break;
                }
                while !q.is_empty() {
                    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
                    if !self.to_host(&f, fds) {
                        return false;
                    }
                    moved = true;
                }
            }
            let mut q = self.h.take_units();
            while !q.is_empty() {
                let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
                // The guest's transport gives it a descriptor per desc.
                let fds = fds
                    .into_iter()
                    .map(|f| f.or_else(|| Some(memfd(4096))))
                    .collect();
                if self.g.from_channel(&f, fds, &mut Plat).is_err() {
                    return false;
                }
                moved = true;
            }
            assert_eq!(self.h.channel_backlog(), 0, "a backlog with nothing queued");
            drop(self.g.local_out().drain());
            drop(self.h.local_out().drain());
            drop(self.g.take_closed_streams());
            drop(self.h.take_closed_streams());
            if !moved {
                break;
            }
        }
        true
    }
}

pub fn run(data: &[u8]) {
    super::sandboxed();
    let fds_before = super::open_fds();
    {
        let mut b = Bytes::new(data);
        let (cfg, rate) = (b.u8(), b.u8());
        let mut p = engines(cfg, rate);
        if !p.pump() {
            return;
        }
        let mut n = 0;
        while !b.is_empty() && n < 128 {
            n += 1;
            let (gdir, hdir) = p.local_dirs();
            let ok = match b.u8() % 9 {
                // The app, raw bytes.
                0 => {
                    let data = b.chunk(4096).to_vec();
                    let fds: VecDeque<OwnedFd> = (0..b.u8() % 3)
                        .map(|_| some_fd(b.u8(), u64::from(b.u16())))
                        .collect();
                    p.g.from_local(&mut LocalIn::new(data, fds), &mut Plat)
                        .is_ok()
                }
                // The app, a message of the protocol.
                1 => {
                    let mut fds = VecDeque::new();
                    let m = build(&mut b, p.g.objects(), gdir, &mut p.ids, &mut fds);
                    match m {
                        Some(m) => p.g.from_local(&mut LocalIn::new(m, fds), &mut Plat).is_ok(),
                        None => true,
                    }
                }
                // The compositor, raw.
                2 => {
                    let data = b.chunk(4096).to_vec();
                    let fds: VecDeque<OwnedFd> = (0..b.u8() % 3)
                        .map(|_| some_fd(b.u8(), u64::from(b.u16())))
                        .collect();
                    p.h.from_local(&mut LocalIn::new(data, fds), &mut Plat)
                        .is_ok()
                }
                // The compositor, a message of the protocol.
                3 => {
                    let mut fds = VecDeque::new();
                    let m = build(&mut b, p.h.objects(), hdir, &mut p.ids, &mut fds);
                    match m {
                        Some(m) => p.h.from_local(&mut LocalIn::new(m, fds), &mut Plat).is_ok(),
                        None => true,
                    }
                }
                // The guest's channel into the host engine: raw, then built.
                4 => {
                    let f = b.chunk(1 << 16).to_vec();
                    p.to_host(&f, Vec::new())
                }
                5 => {
                    let (f, fds) = record_frame(&mut b);
                    if b.u8() & 1 == 0 {
                        p.to_host(&f, fds)
                    } else {
                        let fds = fds
                            .into_iter()
                            .map(|f| f.or_else(|| Some(memfd(8192))))
                            .collect();
                        p.g.from_channel(&f, fds, &mut Plat).is_ok()
                    }
                }
                6 => p.pump(),
                // Time passes: up to about a minute, for the throttle.
                7 => {
                    p.elapsed += Duration::from_millis(u64::from(b.u16()));
                    true
                }
                _ => {
                    let side = b.u8();
                    let e = if side & 1 == 0 { &mut p.g } else { &mut p.h };
                    for i in e.stream_interest() {
                        e.stream_io(i.id, i.read, i.write);
                    }
                    if side & 2 != 0 {
                        e.shed();
                    }
                    true
                }
            };
            p.check_memory();
            // Every descriptor the run holds between operations is one an
            // engine counts: an owner budgets descriptors by that count
            // (the guest daemon's share of its limit among clients).
            let held = p.g.held_fds() + p.h.held_fds();
            let open = super::open_fds();
            assert!(
                open <= fds_before + held,
                "{} descriptors open, {held} counted by the engines",
                open - fds_before
            );
            if !ok {
                break;
            }
        }
        let _ = p.pump();
        p.check_memory();
        let _ = (p.g.shm_stats(), p.h.blob_stats(), p.h.stream_stats());
    }
    let fds_after = super::open_fds();
    assert!(
        fds_after <= fds_before,
        "{} descriptor(s) left open after both engines were dropped",
        fds_after - fds_before
    );
}

/// The channel's frame decoder and the Wayland wire decoder alone: a frame
/// is decoded and walked record by record; the rest is walked as a stream of
/// messages (`wire::Messages`), scanned for lease-device globals as the
/// backend's reader scans the compositor's bytes (`probe::lease_globals`),
/// and decoded as one message against a signature picked by the first bytes.
pub fn codec(data: &[u8]) {
    let mut b = Bytes::new(data);
    let f = b.chunk(1 << 16);
    if let Ok(frame) = frame::decode(f) {
        for r in frame.records() {
            if r.ty == frame::REC_HELLO {
                let _ = frame::Hello::decode(r.payload);
            }
        }
    }
    let i = b.u16() as usize % proto::INTERFACES.len();
    let dir = if b.u8() & 1 == 0 {
        Dir::Request
    } else {
        Dir::Event
    };
    let msgs = proto::iface(i as u16).messages(dir);
    if msgs.is_empty() {
        return;
    }
    let m = &msgs[b.u8() as usize % msgs.len()];
    let msg = b.rest();
    // The same bytes as a stream of messages: each whole one yielded is
    // exactly its size, a bad size ends the walk, and what was yielded and
    // what is left are the input. Then as the compositor's input the
    // reader thread scans for lease devices before the engine
    // (`probe::lease_globals`): one name per message at most.
    let mut walk = wlwire::wire::Messages::new(msg);
    let mut whole = 0;
    for r in &mut walk {
        let Ok((h, bytes)) = r else { break };
        assert_eq!(bytes.len(), h.size as usize);
        assert!((8..=wlwire::wire::MAX_MSG).contains(&bytes.len()));
        whole += 1;
    }
    assert_eq!(walk.consumed() + walk.rest().len(), msg.len());
    assert!(crate::wl::probe::lease_globals(msg).len() <= whole);
    if let Some(h) = wlwire::wire::peek_header(msg) {
        let _ = h;
        if let Ok(args) = wlwire::wire::parse(m, msg) {
            assert_eq!(args.len(), m.args.len());
            for a in &args {
                assert!(a.off <= msg.len(), "an argument past the message");
            }
        }
    }
}
