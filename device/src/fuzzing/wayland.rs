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
//! - stream readiness.
//!
//! A `Fatal` from an end is that connection closed, and ends the run, as it
//! would the connection. Anything else wrong -- a panic, a descriptor still
//! open after both ends are dropped, memory without bound -- is the finding.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use wlwire::engine::{DevPair, Engine, EngineConfig, Local, Platform, Rewrites, Side};
use wlwire::frame::{self, Desc, DescOut, Unit};
use wlwire::policy::{LeaseGate, Policy};
use wlwire::proto::{self, ArgKind, Dir};
use wlwire::shm::ShmBudget;
use wlwire::wire::{MsgBuilder, SERVER_ID_START};

use super::Bytes;

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
}

fn engines(cfg: u8) -> Pair {
    let rewrites = Rewrites {
        devmap: vec![DevPair {
            host: (226, 129),
            guest: (226, 128),
        }],
        clock_offset: Arc::new(AtomicI64::new(i64::from(cfg as i8) * 1_000_000)),
    };
    let mut g = Engine::new(EngineConfig {
        side: Side::Guest,
        local: Local::Client,
        policy: Policy {
            drm_file: true,
            lease: LeaseGate::Allow,
            fences: cfg & 2 != 0,
        },
        rewrites: Some(rewrites),
        synth_released: true,
    });
    let mut h = Engine::new(EngineConfig {
        side: Side::Host,
        local: Local::Server,
        policy: Policy {
            drm_file: cfg & 4 != 0,
            lease: if cfg & 8 != 0 {
                LeaseGate::Allow
            } else {
                LeaseGate::Deny
            },
            fences: cfg & 2 != 0,
        },
        rewrites: None,
        synth_released: false,
    });
    // The VM's budget and a process's, as the backend gives every connection.
    h.set_shm_budget(Arc::new(ShmBudget::new(64 << 20, 64)));
    h.set_shm_budget(Arc::new(ShmBudget::new(16 << 20, 16)));
    g.hello(
        frame::HELLO_G_DRM_FILE
            | if cfg & 2 != 0 {
                frame::HELLO_G_SYNCOBJ
            } else {
                0
            },
    );
    h.hello(0);
    Pair {
        g,
        h,
        ids: (2, SERVER_ID_START),
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
        let (id, arg) = (b.u32() % 64, b.u32());
        let n = b.u16() as usize % 8192;
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
    /// Move everything queued on either side across until quiet (bounded).
    fn pump(&mut self) -> bool {
        for _ in 0..16 {
            let mut moved = false;
            let mut q = self.g.take_units();
            while !q.is_empty() {
                let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
                if self.h.from_channel(&f, fds, &mut Plat).is_err() {
                    return false;
                }
                moved = true;
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
            drop(self.g.local_out().drain());
            drop(self.h.local_out().drain());
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
        let mut p = engines(b.u8());
        if !p.pump() {
            return;
        }
        let mut n = 0;
        while !b.is_empty() && n < 128 {
            n += 1;
            let ok = match b.u8() % 8 {
                // The app, raw bytes.
                0 => {
                    let mut data = b.chunk(4096).to_vec();
                    let mut fds: VecDeque<OwnedFd> = (0..b.u8() % 3)
                        .map(|_| some_fd(b.u8(), u64::from(b.u16())))
                        .collect();
                    p.g.from_local(&mut data, &mut fds, &mut Plat).is_ok()
                }
                // The app, a message of the protocol.
                1 => {
                    let mut fds = VecDeque::new();
                    let m = build(&mut b, p.g.objects(), Dir::Request, &mut p.ids, &mut fds);
                    match m {
                        Some(mut m) => p.g.from_local(&mut m, &mut fds, &mut Plat).is_ok(),
                        None => true,
                    }
                }
                // The compositor, raw.
                2 => {
                    let mut data = b.chunk(4096).to_vec();
                    let mut fds: VecDeque<OwnedFd> = (0..b.u8() % 3)
                        .map(|_| some_fd(b.u8(), u64::from(b.u16())))
                        .collect();
                    p.h.from_local(&mut data, &mut fds, &mut Plat).is_ok()
                }
                // The compositor, a message of the protocol.
                3 => {
                    let mut fds = VecDeque::new();
                    let m = build(&mut b, p.h.objects(), Dir::Event, &mut p.ids, &mut fds);
                    match m {
                        Some(mut m) => p.h.from_local(&mut m, &mut fds, &mut Plat).is_ok(),
                        None => true,
                    }
                }
                // The guest's channel into the host engine: raw, then built.
                4 => {
                    let f = b.chunk(1 << 16).to_vec();
                    p.h.from_channel(&f, Vec::new(), &mut Plat).is_ok()
                }
                5 => {
                    let (f, fds) = record_frame(&mut b);
                    if b.u8() & 1 == 0 {
                        p.h.from_channel(&f, fds, &mut Plat).is_ok()
                    } else {
                        let fds = fds
                            .into_iter()
                            .map(|f| f.or_else(|| Some(memfd(8192))))
                            .collect();
                        p.g.from_channel(&f, fds, &mut Plat).is_ok()
                    }
                }
                6 => p.pump(),
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
            if !ok {
                break;
            }
        }
        let _ = p.pump();
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
/// is decoded and walked record by record; the rest is a message decoded
/// against a signature picked by the first bytes.
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
