// SPDX-License-Identifier: Apache-2.0
//! The capture-injection socket's packets and INJECT_OPEN (inject.rs),
//! against the fake nvidia-drm of `inject::fake`.
//!
//! The input is a sequence of operations: a helper packet (parsed by
//! `protocol::inject::parse_request`, then served by the registry with
//! descriptors the input chooses -- dma-bufs of objects of any type, size,
//! device and offset, the same object again, or no dma-buf at all), a
//! helper's hangup, a guest's INJECT_OPEN through the dispatcher (a known id
//! with its token, the token altered, or any bytes), and a guest file's
//! close. After every step: the registry holds no more than its bounds; an
//! open succeeds only with a live id's own token, and a wrong token gets
//! the same ENOENT as a missing id; an id's mmap range stays read-only
//! while it or an open of it lives. At the end, with everything dropped,
//! no descriptor is left open.

#![forbid(unsafe_code)]

use std::os::fd::OwnedFd;
use std::sync::Arc;

use protocol::inject::{InjRequest, parse_request};
use protocol::messages::*;

use super::Bytes;
use crate::hostfd::{HandleKind, NV_GEM_OBJECT_DMABUF, NV_GEM_OBJECT_NVKMS};
use crate::inject::fake::{FakeHost, Obj};
use crate::inject::{MAX_OPENS, Registry};
use crate::nvidia::NvidiaBackend;

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
fn plane(b: &mut Bytes<'_>, host: &FakeHost, prev: &[OwnedFd]) -> OwnedFd {
    match b.u8() % 5 {
        0 if !prev.is_empty() => prev[b.u8() as usize % prev.len()].try_clone().unwrap(),
        1 => host.not_dmabuf(),
        k => host.dmabuf(Obj {
            ty: match k {
                2 => NV_GEM_OBJECT_DMABUF,
                _ => NV_GEM_OBJECT_NVKMS,
            },
            size: u64::from(b.u32() % (64 << 20)),
            gpu: u32::from(b.u8() % 3),
            offset: b.u64() & !0xfff,
        }),
    }
}

fn inject_open(be: &mut NvidiaBackend, render: u32, id: u64, token: &[u8; 16], tgid: u32) -> i32 {
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
    status(&call(be, MsgType::HostOp, 0, &body))
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
        let mut renders: Vec<(u32, u32)> = Vec::new();
        let mut known: Vec<(u32, [u8; 16])> = Vec::new();
        let mut kept: Vec<OwnedFd> = Vec::new();

        let mut steps = 0;
        while !b.is_empty() && steps < 256 {
            steps += 1;
            let peer = u64::from(b.u8() % 3);
            match b.u8() % 6 {
                0 | 1 => match parse_request(b.chunk(80)) {
                    Ok(InjRequest::Import(imp)) => {
                        let n = b.u8() % 6;
                        let mut fds = Vec::new();
                        for _ in 0..n {
                            let f = plane(b, &host, &kept);
                            kept.push(f.try_clone().unwrap());
                            fds.push(f);
                        }
                        if let Ok((id, tok)) = reg.import(peer, &imp, fds) {
                            known.push((id, tok));
                        }
                    }
                    Ok(InjRequest::Release(r)) => {
                        let _ = reg.release(peer, r.id);
                    }
                    Ok(InjRequest::Hello(_)) | Err(_) => {}
                },
                2 => {
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
                        let st = inject_open(&mut be, render, u64::from(id), &tok, tgid);
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
                _ => {
                    // Any bytes as an INJECT_OPEN.
                    let mut req = HostOpReq {
                        op: OP_INJECT_OPEN,
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
            assert!(be.inject.opens() as u64 <= MAX_OPENS, "past the open bound");
        }
        // Everything the helper imported goes with it.
        for p in 0..3 {
            reg.release_peer(p);
        }
        assert_eq!((reg.live(), reg.bytes()), (0, 0));
        be.session_reset("fuzz end");
        assert_eq!(be.inject.opens(), 0);
    }
    assert_eq!(super::open_fds(), before, "a descriptor was leaked");
}
