// SPDX-License-Identifier: Apache-2.0
//! The capture-injection socket's packets and INJECT_OPEN (inject.rs),
//! against the fake nvidia-drm of `inject::fake`.
//!
//! The input is a sequence of operations: a helper packet (built or raw,
//! through `inject::serve_packet`, every framing rule of the socket, with
//! descriptors the input chooses -- dma-bufs of objects of any type, size,
//! device and offset, the same object again, syncobj files, or nothing), a
//! helper's hangup, a guest's INJECT_OPEN and INJECT_OPEN_SYNCOBJ through the
//! dispatcher (a known id with its token, the token altered, or any bytes),
//! an MMAP of an injected object's range, a GEM_CLOSE and a PRIME_EXPORT of
//! an open's handle, and a guest file's close. After every step: the
//! registry holds no more than its bounds; an open succeeds only with a live
//! id's own token, and a wrong token gets the same ENOENT as a missing id; a
//! placement of a range an id or an open holds is read-only, placed and
//! reported so; an open's handle is never exported, and a GEM_CLOSE of it
//! forgets it. At the end, with everything dropped, no descriptor is left
//! open.

#![forbid(unsafe_code)]

use std::os::fd::OwnedFd;
use std::sync::Arc;

use protocol::inject::{
    INJ_OP_IMPORT, INJ_OP_IMPORT_SYNCOBJ, INJ_VERSION, InjHello, InjImport, InjImportSyncobj,
    InjRelease,
};
use protocol::messages::*;

use super::Bytes;
use crate::hostfd::{HandleKind, NV_GEM_OBJECT_DMABUF, NV_GEM_OBJECT_NVKMS};
use crate::inject::fake::{FakeHost, Obj};
use crate::inject::{MAX_OPENS, Registry, Served, serve_packet};
use crate::nvidia::NvidiaBackend;
use crate::privfd::PrivateFd;

fn call(be: &mut NvidiaBackend, t: MsgType, handle: u32, body: &[u8]) -> Vec<u8> {
    let mut msg = crate::session::hdr(t, handle, 0, 1);
    msg.extend_from_slice(body);
    let mut resp = vec![0u8; 4096];
    let n = be.dispatch(&msg, &mut resp);
    resp.truncate(n);
    resp
}

fn status(r: &[u8]) -> i32 {
    r.get(8..12)
        .map_or(0, |s| i32::from_le_bytes(s.try_into().unwrap()))
}

/// A descriptor for one plane, as the input says.
fn plane(b: &mut Bytes<'_>, host: &FakeHost, prev: &[OwnedFd], offs: &mut Vec<u64>) -> OwnedFd {
    match b.u8() % 5 {
        0 if !prev.is_empty() => prev[b.u8() as usize % prev.len()].try_clone().unwrap(),
        1 => host.not_dmabuf(),
        k => {
            let o = Obj {
                ty: match k {
                    2 => NV_GEM_OBJECT_DMABUF,
                    _ => NV_GEM_OBJECT_NVKMS,
                },
                size: u64::from(b.u32() % (64 << 20)),
                gpu: u32::from(b.u8() % 3),
                // Pages, below 2^40: the offsets GEM_MAP_OFFSET gives.
                offset: (u64::from(b.u32()) % (1 << 28)) << 12,
            };
            offs.push(o.offset);
            host.dmabuf(o)
        }
    }
}

/// Every host ioctl answers 0 (a GEM_CLOSE that succeeded).
fn ok_ioctl(_: std::os::fd::RawFd, _: u64, _: &mut crate::sys::block::Arg<'_>) -> i32 {
    0
}

/// A window that takes every placement and remembers whether the last one
/// was writable.
#[derive(Clone, Default)]
struct Window(Arc<std::sync::Mutex<Option<bool>>>);

impl crate::shm::WindowPlacer for Window {
    fn place(
        &self,
        _: u64,
        _: u64,
        _: std::os::fd::RawFd,
        _: u64,
        writable: bool,
    ) -> crate::error::Result<()> {
        *self.0.lock().unwrap() = Some(writable);
        Ok(())
    }
    fn withdraw(&self, _: u64, _: u64) -> crate::error::Result<()> {
        Ok(())
    }
}

/// A helper packet: a well-formed one of each op, from the input's values,
/// or the input's bytes as they are.
fn packet(b: &mut Bytes<'_>) -> Vec<u8> {
    match b.u8() % 6 {
        0 => InjHello {
            version: if b.u8() % 8 == 0 {
                b.u32()
            } else {
                INJ_VERSION
            },
            flags: if b.u8() % 8 == 0 { b.u32() } else { 0 },
        }
        .to_bytes()
        .to_vec(),
        1 | 2 => {
            let mut offsets = [0; 4];
            let mut strides = [0; 4];
            let nplanes = u32::from(b.u8() % 3);
            let (w, h) = (1 + u32::from(b.u16() % 256), 1 + u32::from(b.u16() % 256));
            for i in 0..nplanes as usize {
                offsets[i] = if b.u8() % 4 == 0 { b.u32() } else { 0 };
                strides[i] = if b.u8() % 4 == 0 { b.u32() } else { w * 4 };
            }
            InjImport {
                nplanes,
                width: w,
                height: h,
                fourcc: [0x3432_5258, 0x3231_564e, b.u32()][b.u8() as usize % 3],
                flags: u32::from(b.u8() % 4 == 0),
                modifier: b.u64(),
                offsets,
                strides,
            }
            .to_bytes()
            .to_vec()
        }
        3 => InjRelease { id: b.u32() % 16 }.to_bytes().to_vec(),
        4 => InjImportSyncobj {
            flags: u32::from(b.u8() % 8 == 0),
        }
        .to_bytes()
        .to_vec(),
        _ => b.chunk(80).to_vec(),
    }
}

fn inject_open(be: &mut NvidiaBackend, render: u32, id: u64, token: &[u8; 16], tgid: u32) -> i32 {
    inject_open_gem(be, render, id, token, tgid).0
}

/// INJECT_OPEN: its status and the GEM handle it answered.
fn inject_open_gem(
    be: &mut NvidiaBackend,
    render: u32,
    id: u64,
    token: &[u8; 16],
    tgid: u32,
) -> (i32, u32) {
    let mut req = HostOpReq {
        op: OP_INJECT_OPEN,
        nargs: 4,
        args: [0; OP_MAX_ARGS],
    };
    req.args[0] = u64::from(render);
    req.args[1] = id;
    req.args[2] = u64::from_le_bytes(token[..8].try_into().unwrap());
    req.args[3] = u64::from_le_bytes(token[8..].try_into().unwrap());
    let mut body = crate::sys::pod::bytes(&req).to_vec();
    body.extend_from_slice(crate::sys::pod::bytes(&ProcId {
        start_ns: 1,
        tgid,
        euid: 0,
    }));
    let r = call(be, MsgType::HostOp, 0, &body);
    let h = size_of::<MsgHeader>();
    let gem = r
        .get(h + 8..h + 12)
        .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
    (status(&r), gem)
}

pub fn run(b: &mut Bytes<'_>) {
    super::sandboxed();
    let before = super::open_fds();
    {
        let nodes = 1 + u32::from(b.u8() % 2);
        let host = Arc::new(FakeHost::new(nodes));
        let max_buffers = 1 + b.u8() as usize % 8;
        let max_bytes = u64::from(b.u32()) << 8;
        let reg = Arc::new(Registry::with_limits(host.clone(), max_buffers, max_bytes));
        let mut be = NvidiaBackend::new(crate::shm::ZoneConfig {
            uc_size: 4096 * 2,
            wc_size: 4096 * 4,
            wb_size: 4096 * 2,
        });
        let hello = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: GCAP_PROC_ID,
            uvm_aperture_mib: 0,
        };
        call(&mut be, MsgType::Hello, 0, crate::sys::pod::bytes(&hello));
        be.set_inject(Some(reg.clone()));
        be.set_host_ioctl_for_test(ok_ioctl);
        let window = Window::default();
        be.set_window(Box::new(window.clone()));
        let mut hello = [false; 3];
        let mut offsets: Vec<u64> = Vec::new();
        let mut opened: Vec<(u32, u32)> = Vec::new();
        let mut renders: Vec<(u32, u32)> = Vec::new();
        let mut known: Vec<(u32, [u8; 16])> = Vec::new();
        let mut kept: Vec<OwnedFd> = Vec::new();
        let mut syncobjs: Vec<(u32, [u8; 16])> = Vec::new();

        let mut steps = 0;
        while !b.is_empty() && steps < 256 {
            steps += 1;
            let peer = u64::from(b.u8() % 3);
            match b.u8() % 9 {
                0 | 1 => {
                    let bytes = packet(b);
                    let n = b.u8() % 5;
                    let mut fds = Vec::new();
                    for _ in 0..n {
                        let f = if b.u8() % 4 == 0 {
                            host.syncobj()
                        } else {
                            plane(b, &host, &kept, &mut offsets)
                        };
                        kept.push(f.try_clone().unwrap());
                        fds.push(PrivateFd::new(f));
                    }
                    let truncated = b.u8() % 32 == 0;
                    let p = peer as usize;
                    match serve_packet(&reg, peer, &mut hello[p], &bytes, truncated, fds) {
                        Served::Reply(r) => {
                            if r.status == 0 && r.op == INJ_OP_IMPORT {
                                known.push((r.id, r.token));
                            }
                            if r.status == 0 && r.op == INJ_OP_IMPORT_SYNCOBJ {
                                syncobjs.push((r.id, r.token));
                            }
                        }
                        Served::Last(_) | Served::Hangup => {
                            hello[p] = false;
                            reg.release_peer(peer);
                        }
                    }
                }
                2 => {
                    hello[peer as usize] = false;
                    reg.release_peer(peer);
                }
                3 => {
                    // A render file of some node, or an open with one.
                    if renders.is_empty() || b.u8() % 4 == 0 {
                        let gpu = u32::from(b.u8()) % nodes;
                        let h =
                            be.adopt_for_test(host.render_file(gpu), HandleKind::DriRender(gpu));
                        renders.push((h, gpu));
                    }
                    let (render, gpu) = renders[b.u8() as usize % renders.len()];
                    let tgid = u32::from(b.u8() % 3);
                    if known.is_empty() || b.u8() % 5 == 0 {
                        let mut tok = [0u8; 16];
                        tok.copy_from_slice(&{
                            let t = b.take(16);
                            let mut a = [0u8; 16];
                            a[..t.len()].copy_from_slice(t);
                            a
                        });
                        let st = inject_open(&mut be, render, b.u64(), &tok, tgid);
                        // Tokens are random: guessing one is a finding.
                        assert!(st != 0 || known.iter().any(|(_, t)| *t == tok));
                    } else {
                        let (id, mut tok) = known[b.u8() as usize % known.len()];
                        let wrong = b.u8() % 3 == 0;
                        if wrong {
                            tok[b.u8() as usize % 16] ^= 1 | b.u8();
                        }
                        let live = reg.open(id, &tok, gpu);
                        let (st, gem) = inject_open_gem(&mut be, render, u64::from(id), &tok, tgid);
                        if st == 0 {
                            opened.push((render, gem));
                        }
                        match live {
                            Ok(_) => assert!(
                                st == 0 || st == -libc::EAGAIN,
                                "a live id with its token: {st}"
                            ),
                            Err(e) => assert_eq!(st, -e, "the registry and INJECT_OPEN disagree"),
                        }
                        if wrong {
                            assert_eq!(st, -libc::ENOENT, "a wrong token opened something");
                        }
                    }
                }
                4 => {
                    if !renders.is_empty() {
                        let i = b.u8() as usize % renders.len();
                        let (h, _) = renders.swap_remove(i);
                        call(&mut be, MsgType::Close, h, &[]);
                    }
                }
                5 if !syncobjs.is_empty() && !renders.is_empty() && b.u8() % 2 == 0 => {
                    // A syncobj: its token, or one altered.
                    let (id, mut tok) = syncobjs[b.u8() as usize % syncobjs.len()];
                    let wrong = b.u8() % 3 == 0;
                    if wrong {
                        tok[b.u8() as usize % 16] ^= 1 | b.u8();
                    }
                    let (render, _) = renders[b.u8() as usize % renders.len()];
                    let live = reg.open_syncobj(id, &tok).is_ok();
                    let mut req = HostOpReq {
                        op: OP_INJECT_OPEN_SYNCOBJ,
                        nargs: 4,
                        args: [0; OP_MAX_ARGS],
                    };
                    req.args[0] = u64::from(render);
                    req.args[1] = u64::from(id);
                    req.args[2] = u64::from_le_bytes(tok[..8].try_into().unwrap());
                    req.args[3] = u64::from_le_bytes(tok[8..].try_into().unwrap());
                    let st = status(&call(
                        &mut be,
                        MsgType::HostOp,
                        0,
                        crate::sys::pod::bytes(&req),
                    ));
                    assert_eq!(
                        st == 0,
                        live,
                        "INJECT_OPEN_SYNCOBJ and the registry disagree: {st}"
                    );
                    if wrong {
                        assert_eq!(st, -libc::ENOENT, "a wrong token opened a syncobj");
                    }
                }
                6 if !renders.is_empty() => {
                    // An MMAP of an injected range (or near one).
                    let (render, _) = renders[b.u8() as usize % renders.len()];
                    let off = if offsets.is_empty() || b.u8() % 4 == 0 {
                        u64::from(b.u32()) << 12
                    } else {
                        offsets[b.u8() as usize % offsets.len()]
                    };
                    let size = 4096 * (1 + u64::from(b.u8() % 4));
                    let ro = be.inject.read_only(off, size);
                    *window.0.lock().unwrap() = None;
                    let req = MmapReq {
                        size,
                        offset: off,
                        prot: 3,
                        padding: 0,
                    };
                    let r = call(&mut be, MsgType::Mmap, render, crate::sys::pod::bytes(&req));
                    if ro && status(&r) == 0 {
                        let m: MmapResp =
                            crate::sys::pod::read(&r, size_of::<MsgHeader>()).unwrap_or_default();
                        assert_ne!(
                            m.flags & MMAP_F_READ_ONLY,
                            0,
                            "an injected range told writable"
                        );
                        assert_ne!(
                            *window.0.lock().unwrap(),
                            Some(true),
                            "an injected range placed writable"
                        );
                    }
                }
                7 if !opened.is_empty() => {
                    // A GEM_CLOSE, or a PRIME_EXPORT, of an open's handle.
                    let (render, gem) = opened[b.u8() as usize % opened.len()];
                    if b.u8() % 2 == 0 {
                        let was = be.inject.is_open(render, gem);
                        let mut close = [0u8; 8];
                        close[..4].copy_from_slice(&gem.to_le_bytes());
                        let mut req = Vec::new();
                        for v in [
                            MsgType::Ioctl as u32,
                            render,
                            0,
                            0,
                            crate::hostfd::DRM_IOCTL_GEM_CLOSE,
                            8,
                            0,
                            0,
                            0,
                            0,
                        ] {
                            req.extend_from_slice(&v.to_le_bytes());
                        }
                        req.extend_from_slice(&close);
                        let mut resp = vec![0u8; 4096];
                        let n = be.dispatch(&req, &mut resp);
                        if was && status(&resp[..n]) == 0 {
                            assert!(!be.inject.is_open(render, gem), "a closed open is kept");
                        }
                    } else if be.inject.is_open(render, gem) {
                        let mut req = HostOpReq {
                            op: OP_PRIME_EXPORT,
                            nargs: 2,
                            args: [0; OP_MAX_ARGS],
                        };
                        req.args[0] = u64::from(render);
                        req.args[1] = u64::from(gem);
                        let st = status(&call(
                            &mut be,
                            MsgType::HostOp,
                            0,
                            crate::sys::pod::bytes(&req),
                        ));
                        assert_eq!(st, -libc::EINVAL, "an open's handle was exported");
                    }
                }
                _ => {
                    // Any bytes as an INJECT_OPEN or INJECT_OPEN_SYNCOBJ.
                    let mut req = HostOpReq {
                        op: if b.u8() % 2 == 0 {
                            OP_INJECT_OPEN
                        } else {
                            OP_INJECT_OPEN_SYNCOBJ
                        },
                        nargs: u32::from(b.u8() % 8),
                        args: [0; OP_MAX_ARGS],
                    };
                    for a in &mut req.args {
                        *a = b.u64();
                    }
                    call(&mut be, MsgType::HostOp, 0, crate::sys::pod::bytes(&req));
                }
            }
            assert!(reg.live() <= max_buffers, "past the buffer bound");
            assert!(reg.bytes() <= max_bytes, "past the byte bound");
            assert!(
                reg.syncobjs() <= crate::inject::MAX_SYNCOBJS,
                "past the syncobj bound"
            );
            assert!(be.inject.opens() as u64 <= MAX_OPENS, "past the open bound");
        }
        // Everything the helper imported goes with it.
        for p in 0..3 {
            reg.release_peer(p);
        }
        assert_eq!((reg.live(), reg.bytes(), reg.syncobjs()), (0, 0, 0));
        be.session_reset("fuzz end");
        assert_eq!(be.inject.opens(), 0);
    }
    // A render file's last close is the closer thread's (closer.rs).
    crate::closer::wait_idle(std::time::Duration::from_secs(5));
    assert_eq!(super::open_fds(), before, "a descriptor was leaked");
}
