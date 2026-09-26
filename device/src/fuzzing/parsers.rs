// SPDX-License-Identifier: Apache-2.0
//! The backend's parsers one at a time, each against what it promises.
//!
//! The first byte picks the parser; the rest is its input. Where a parser
//! promises something checkable from outside -- deep segments point only at
//! buffers of the backend's holding exactly the guest's bytes, a page list
//! is the pages it lists, a pointer scrub leaves no guest pointer where RM
//! looks -- that is checked against an independent reading of the wire
//! format, not just that nothing panics.

#![forbid(unsafe_code)]

use protocol::messages::*;

use super::{Bytes, backend, host};
use crate::{deepseg, fence, guestptr, kms, osdesc, rmctl, rmshare};

fn rd32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4)
        .map_or(0, |v| u32::from_le_bytes(v.try_into().unwrap()))
}
fn rd64(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8)
        .map_or(0, |v| u64::from_le_bytes(v.try_into().unwrap()))
}

pub fn run(data: &[u8]) {
    super::sandboxed();
    let mut b = Bytes::new(data);
    match b.u8() % 8 {
        0 => deep_segments(&mut b),
        1 => os_descriptor(&mut b),
        2 => rm_share(&mut b),
        3 => nvkms_v1(&mut b),
        4 => pointer_scrub(&mut b),
        5 => small(&mut b),
        6 => ownership(&mut b),
        _ => idle_channels(&mut b),
    }
}

/// `deepseg::Segments::relocate` against one of the controls whose pointers
/// RM sizes (`abi::rmctrl::DEEP_CONTROLS`).
pub fn deep_segments(b: &mut Bytes) {
    let controls = abi::rmctrl::DEEP_CONTROLS;
    let c = &controls[b.u8() as usize % controls.len()];
    let block_len = b.u16() as usize % 1024;
    let mut block = b.take(block_len).to_vec();
    block.resize(block_len, 0);
    let deep = b.rest();
    let mut a = crate::sys::block::Arena::new();
    let Ok(blk) = a.block(&block, block_len) else {
        return;
    };
    match deepseg::Segments::relocate("fuzz", c.ptrs, &mut a, blk, deep) {
        Err(_) => assert_eq!(a.bytes(blk), &block[..], "a refused relocation changed the block"),
        Ok(segs) => {
            // The segments as the guest laid them out, read independently,
            // and read through the pointers as RM would (`Follow`).
            let count = rd32(deep, 0) as usize;
            let mut at = 8 + count * 8;
            let mut expect = Vec::new();
            for i in 0..count {
                let (ptr, len) = (
                    rd32(deep, 8 + i * 8) as usize,
                    rd32(deep, 12 + i * 8) as usize,
                );
                let rule = c
                    .ptrs
                    .iter()
                    .find(|r| r.ptr == ptr)
                    .expect("a pointer RM follows");
                assert_eq!(
                    rule.size(a.bytes(blk)).map(|s| s as usize),
                    Some(len),
                    "RM's size"
                );
                expect.push((ptr, deep[at..at + len].to_vec()));
                at += len;
            }
            assert_eq!(at, deep.len());
            if block_len > 0 {
                host::reset(&[], None);
                let follow = Follow(expect);
                assert!(a.call(&follow, -1, 0, blk) >= 0, "a segment not mapped");
            }
            assert_eq!(a.reply(blk), block, "the reply gives the caller's block back");
            assert_eq!(
                segs.reply(&a),
                deep,
                "the reply is laid out as the request, the host having written nothing"
            );
        }
    }
}

/// A host that reads each relocated pointer of the block it is handed and
/// checks the segment behind it holds the guest's bytes.
struct Follow(Vec<(usize, Vec<u8>)>);

impl crate::sys::block::Kernel for Follow {
    fn ioctl(&self, _: std::os::fd::RawFd, _: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        for (i, (ptr, want)) in self.0.iter().enumerate() {
            let p = rd64(arg.bytes(), *ptr);
            assert!(
                host::follow(arg, p, want.len() as u64, "a relocated segment"),
                "a segment not mapped"
            );
            assert_eq!(
                arg.read(p, want.len()).as_deref(),
                Some(&want[..]),
                "segment {i} holds other bytes"
            );
        }
        0
    }
}

/// `osdesc::describe`, `parse_runs`, `resolve` and `map`: the pages RM is
/// handed are the pages the list names.
pub fn os_descriptor(b: &mut Bytes) {
    use crate::hostfd::{IOC_RW, ioc};
    let (cmd, outer_len) = match b.u8() % 3 {
        0 => (
            ioc(IOC_RW, b'F', abi::ioctl::NV_ESC_RM_ALLOC_MEMORY, 56),
            56,
        ),
        1 => (
            ioc(IOC_RW, b'F', abi::ioctl::NV_ESC_RM_VID_HEAP_CONTROL, 184),
            184,
        ),
        _ => (ioc(IOC_RW, b'F', abi::ioctl::NV_ESC_RM_ALLOC, 48), 48),
    };
    let mut outer = b.take(outer_len).to_vec();
    outer.resize(outer_len, 0);
    let nested_len = if outer_len == 48 { 40 } else { 0 };
    let mut nested = b.take(nested_len).to_vec();
    nested.resize(nested_len, 0);
    // Steer the class/function so the three shapes are reached.
    match outer_len {
        56 => outer[12..16].copy_from_slice(&0x71u32.to_le_bytes()),
        184 => outer[8..12].copy_from_slice(&27u32.to_le_bytes()),
        _ => outer[12..16].copy_from_slice(&0x71u32.to_le_bytes()),
    }
    let deep = b.rest();
    let Ok(call) = osdesc::describe(cmd, &outer, &nested) else {
        return;
    };
    let Ok(runs) = osdesc::parse_runs(&call, deep) else {
        return;
    };
    // The list, read independently.
    let mut pages = Vec::new();
    for i in 0..rd32(deep, 0) as usize {
        let gpa = rd64(deep, 8 + i * 16);
        for k in 0..u64::from(rd32(deep, 16 + i * 16)) {
            pages.push(gpa + k * 4096);
        }
    }
    let flat: Vec<u64> = runs
        .iter()
        .flat_map(|&(g, n)| (0..n).map(move |k| g + k * 4096))
        .collect();
    assert_eq!(flat, pages, "the runs are the list");
    assert_eq!(pages.len() as u64, call.pages());
    let ram = backend::guest_ram();
    let Ok(res) = osdesc::resolve(&ram, &runs) else {
        return;
    };
    assert_eq!(res.bytes(), call.pages() * 4096);
    let Ok(pinned) = res.map(call.in_page(), call.writable) else {
        return;
    };
    assert_eq!(pinned.addr % 4096, call.in_page());
    assert_eq!(pinned.span.addr_at(pinned.at), Some(pinned.addr));
    for (i, gpa) in pages.iter().enumerate() {
        for off in [0u64, 4088] {
            let at = pinned.at - call.in_page() + i as u64 * 4096 + off;
            let got = u64::from_le_bytes(pinned.span.read(at, 8).try_into().unwrap());
            assert_eq!(
                got,
                host::ram_word(gpa + off),
                "page {i} is not guest page {gpa:#x}"
            );
        }
    }
}

/// The share and duplicate parsers, and the named-client tables.
pub fn rm_share(b: &mut Bytes) {
    let escape = [0x34u32, 0x35, 0x2a, 0x2b][b.u8() as usize % 4];
    let cmd = b.u32();
    let class = b.u32();
    let params = b.rest();
    let _ = rmshare::Policy::read(params, 0);
    let _ = rmshare::dup_names(params);
    let _ = rmshare::alloc_named(class, params);
    let _ = rmshare::control_named(cmd, params);
    if let Some((owner, _, p)) = rmshare::share_of(escape, params) {
        let _ = rmshare::classify_share(owner, &p, |c| c & 1 == 0);
        let mut list = Vec::new();
        rmshare::apply_share(&mut list, owner, &p);
    }
    if let Some(at) = rmshare::status_at(escape) {
        let r = rmshare::refusal(params, at, 0x1b);
        assert_eq!(r.len(), params.len());
    }
    let _ = rmctl::host_pid_control(params);
    let _ = rmctl::refusal(params);
    let _ = rmctl::unix_control(cmd);
    let _ = rmctl::unix_refused(params);
    let _ = rmctl::unsupported(params);
}

/// NVKMS on a v1 message: the policy's in-place checks and rewrites.
pub fn nvkms_v1(b: &mut Bytes) {
    let p = crate::nvkms::NvkmsPolicy::new();
    let versions = [
        "535.129.03",
        "580.178.04",
        "595.99.02",
        "610.57.04",
        "615.71.09",
        "999.1.1",
    ];
    let v = versions[b.u8() as usize % versions.len()];
    p.set_version(abi::version::DriverVersion::parse(v).unwrap());
    p.set_kms_card(b.u8() & 1 != 0);
    p.set_coherent_display(b.u8() & 1 != 0);
    let target = u32::from(b.u8() % 4);
    // A few calls in a row, so records made by one are read by the next.
    for _ in 0..4 {
        let len = b.u16() as usize % 4096;
        let mut msg = b.take(len).to_vec();
        if msg.is_empty() {
            break;
        }
        if p.v1_before(target, &mut msg).is_ok() && !p.v1_cached(target, &mut msg) {
            p.v1_record(target, &msg);
            p.v1_after(&mut msg);
        }
    }
    p.forget_handle(target);
    p.reset();
}

/// `guestptr::rm_escape` and `scrub_control`: in the host's copy they
/// build, no pointer field RM follows holds a value the guest sent, and the
/// caller's values come back in the reply.
pub fn pointer_scrub(b: &mut Bytes) {
    use crate::sys::block::Arena;
    let cmd = b.u32();
    let ctl = b.u32();
    let len = b.u16() as usize % 512;
    let params = b.take(len).to_vec();
    if let Ok(plan) = guestptr::rm_escape(cmd, &params) {
        let mut a = Arena::new();
        if let Ok(top) = a.block(&params, params.len()) {
            if plan.declare(&mut a, top, &|_| None).is_ok() {
                let mut want = params.clone();
                for &(off, s) in &plan.slots {
                    assert_eq!(rd64(a.bytes(top), off), 0, "escape {cmd:#x}: {off} left for RM");
                    // An OUT address is the host's answer, and the host
                    // here wrote nothing.
                    if s == guestptr::TopSlot::Out {
                        want[off..off + 8].fill(0);
                    }
                }
                // Whatever else was taken out is given back.
                assert_eq!(a.reply(top), want);
            }
        }
    }
    let nested = b.rest().to_vec();
    let mut a = Arena::new();
    let Ok(nb) = a.block(&nested, nested.len()) else {
        return;
    };
    guestptr::scrub_control(ctl, &mut a, nb, &[]).expect("nothing else declared");
    for &off in guestptr::control_pointers(ctl) {
        if off + 8 <= nested.len() {
            assert_eq!(
                rd64(a.bytes(nb), off),
                0,
                "control {ctl:#x}: a pointer at {off} left for RM"
            );
        }
    }
    assert_eq!(a.reply(nb), nested, "the caller reads its own pointers back");
    let init_mask = b.u64();
    let _ = guestptr::uvm_gate(ctl & 1 != 0, cmd, &nested, init_mask);
}

/// RM's IDLE_CHANNELS with its lists as deep segments.
pub fn idle_channels(b: &mut Bytes) {
    use crate::hostfd::{IOC_RW, ioc};
    let cmd = ioc(IOC_RW, b'F', abi::ioctl::NV_ESC_RM_IDLE_CHANNELS, 56);
    let mut params = b.take(56).to_vec();
    params.resize(56, 0);
    let deep = b.rest();
    let Ok(plan) = guestptr::idle_channels_list(cmd, &params, deep) else {
        return;
    };
    let mut a = crate::sys::block::Arena::new();
    let Ok(top) = a.block(&params, 56) else {
        return;
    };
    if plan.declare(&mut a, top, &|_| None).is_err() {
        return;
    }
    // RM follows every non-null one for numChannels u32s: each must be a
    // block of the call; the rest are null.
    host::reset(&[], None);
    assert!(a.call(&IdleLists, -1, 0, top) >= 0);
}

/// RmDeprecatedIdleChannels, as far as its three lists go.
struct IdleLists;

impl crate::sys::block::Kernel for IdleLists {
    fn ioctl(&self, _: std::os::fd::RawFd, _: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        let a = arg.bytes().to_vec();
        let count = u64::from(rd32(&a, 12));
        for off in [16usize, 24, 32] {
            let p = rd64(&a, off);
            assert!(
                p == 0 || host::follow(arg, p, count * 4, "an IDLE_CHANNELS list"),
                "a list not mapped"
            );
        }
        0
    }
}

/// The little ones: fence argument rewrites, uevents, KMS property names.
pub fn small(b: &mut Bytes) {
    let cmd = b.u32();
    let mut arg = b.chunk(256).to_vec();
    let _ = fence::before(cmd, &mut arg);
    let rest = b.rest();
    if let Some(ev) = kms::parse_uevent(rest) {
        let cards = [crate::hostfd::CardNode {
            name: "card1".into(),
            major: 226,
            minor: 1,
            render_index: 0,
        }];
        let _ = kms::card_event(&ev, &cards);
    }
    let _ = kms::prop_kind(rest);
}

/// RM object ownership and grants, as a sequence of operations: every
/// forgotten client is gone from every list, and the count is the lists'.
pub fn ownership(b: &mut Bytes) {
    let mut o = rmshare::Ownership::default();
    let mut n = 0;
    while !b.is_empty() && n < 256 {
        n += 1;
        let op = b.u8();
        let (x, y, z) = (
            u32::from(b.u8() % 8),
            u32::from(b.u8() % 8),
            u32::from(b.u8() % 8),
        );
        match op % 6 {
            0 => {
                let id = ProcId {
                    start_ns: u64::from(b.u8() % 3),
                    tgid: u32::from(b.u8() % 3),
                    euid: u32::from(b.u8() % 2),
                };
                o.owners
                    .insert(x, rmshare::Caller::from_wire(&id, op & 0x80 != 0));
            }
            1 => {
                let mut p = [0u8; 12];
                p.copy_from_slice(&{
                    let mut v = b.take(12).to_vec();
                    v.resize(12, 0);
                    v
                });
                if let Some(pol) = rmshare::Policy::read(&p, 0) {
                    if !o.full_for(x, y, &pol) {
                        o.shared(x, y, &pol);
                    }
                }
            }
            2 => o.object_freed(x, y),
            3 => {
                o.forget_clients(&[x]);
                assert!(!o.owners.contains_key(&x));
                assert!(
                    o.grants.keys().all(|&(c, _)| c != x),
                    "a forgotten client's grants remain"
                );
                assert!(
                    o.grants.values().flatten().all(|g| g.target != x),
                    "a grant to a forgotten client remains"
                );
            }
            4 => {
                let v = o.dup_verdict(|c| c < 6, x, y, z);
                if v == rmshare::DupVerdict::Allowed && x != y {
                    let same = matches!((o.owners.get(&x), o.owners.get(&y)), (Some(a), Some(c)) if a.same_process(c));
                    let granted = o
                        .grants
                        .iter()
                        .any(|(&(c, _), l)| c == y && l.iter().any(|g| g.target == x));
                    assert!(
                        same || granted,
                        "a duplicate between two processes nothing shares"
                    );
                }
            }
            _ => {
                let named = rmshare::Named {
                    h: y,
                    what: "fuzz",
                    rule: rmshare::Rule::Process,
                    obj: z,
                };
                let caller = o.owners.get(&x).copied();
                if o.named_ok(caller.as_ref(), x, &named) {
                    let a = caller.unwrap();
                    assert!(o.owners.get(&y).is_some_and(|c| c.same_process(&a)));
                }
            }
        }
        let count: usize = o.grants.values().map(|l| 1 + l.len()).sum();
        assert_eq!(o.grant_count, count, "the grant count is not the lists'");
    }
}
