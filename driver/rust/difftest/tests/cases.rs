// SPDX-License-Identifier: GPL-2.0-only
//! The C behaviour's cases, stated: each runs through both implementations
//! (which must agree, as in diff.rs) and then checks what they did against
//! what the C has always done -- the requests byte for byte where it
//! matters, and what the caller reads back.

use nvgpu_guest_difftest::cabi;
use nvgpu_guest_difftest::renv::{BCAP_DEEP_SEGS, BCAP_OS_DESC, BCAP_PROC_ID};
use nvgpu_guest_difftest::scen::{self, Call, DevSpec, Outcome, Scenario, FDS};
use nvgpu_guest_difftest::world::{Ev, Hooks, World};

const ARG: u64 = 0x7f00_0000_0000;
const NESTED: u64 = 0x7f00_0001_0000;
const A: u64 = 0x7f00_0002_0000;
const B: u64 = 0x7f00_0003_0000;
const C: u64 = 0x7f00_0004_0000;

fn dev(caps: u32, fdt: Vec<(u32, u32)>) -> DevSpec {
    DevSpec {
        bad_schema: false,
        version: "610.57.04".into(),
        v2: true,
        caps,
        max_req: 1 << 20,
        max_resp: 1 << 20,
        fdt,
        handle: 5,
    }
}

fn world() -> World {
    World {
        fds: FDS.iter().copied().collect(),
        clock: Some(1_000_000),
        hooks: Hooks {
            mask: 0x3f,
            seed: 1,
            fail_gem: None,
        },
        ..World::default()
    }
}

fn ioc(dir: u32, ty: u8, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (u32::from(ty) << 8) | nr
}

fn put(b: &mut [u8], off: usize, v: u64, width: usize) {
    b[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// Both implementations, which must agree; what they did.
fn run(d: DevSpec, w: World, call: Call) -> Outcome {
    let s = Scenario {
        seed: 0,
        dev: d,
        world: w,
        call,
    };
    scen::diff(&s).unwrap_or_else(|e| panic!("{e}"))
}

fn sends(o: &Outcome) -> Vec<Vec<u8>> {
    o.world
        .events
        .iter()
        .filter_map(|e| {
            if let Ev::Send(b) = e {
                Some(b.clone())
            } else {
                None
            }
        })
        .collect()
}

fn mem(o: &Outcome, at: u64) -> &[u8] {
    &o.world.mem[&at]
}

/// A protocol-v1 reply.
fn reply(status: i32, data: &[u8], nested: &[u8], deep: &[u8]) -> Vec<u8> {
    let mut r = Vec::new();
    for v in [
        3u32,
        0,
        status as u32,
        0,
        data.len() as u32,
        nested.len() as u32,
        deep.len() as u32,
    ] {
        r.extend_from_slice(&v.to_le_bytes());
    }
    r.extend_from_slice(data);
    r.extend_from_slice(nested);
    r.extend_from_slice(deep);
    r
}

/// The request's payload (after the 40-byte header).
fn payload(req: &[u8]) -> &[u8] {
    &req[40..]
}

#[test]
fn a_descriptor_at_a_fixed_offset_is_the_backends_handle_and_comes_back() {
    let d = dev(0, vec![(0x27, 48)]);
    let cmd = ioc(3, b'F', 0x27, 56);
    for (fd, sent, ret) in [
        (3i32, Some(101u32), 0i64),
        (-1, Some(u32::MAX), 0),
        (-2, None, -9),
        (99, None, -9),
    ] {
        let mut w = world();
        let mut arg = vec![0u8; 56];
        put(&mut arg, 12, 0x3e, 4);
        put(&mut arg, 48, fd as u32 as u64, 4);
        w.mem.insert(ARG, arg.clone());
        // The backend answers with the handle where the descriptor was.
        let mut back = arg.clone();
        put(&mut back, 48, u64::from(sent.unwrap_or(0)), 4);
        put(&mut back, 0, 0x1234, 4);
        w.canned = vec![reply(0, &back, &[], &[])];
        let o = run(d.clone(), w, Call::Fd { cmd, arg: ARG });
        assert_eq!(o.ret, ret, "fd {fd}");
        match sent {
            Some(h) => {
                let s = sends(&o);
                assert_eq!(le32(payload(&s[0]), 48), h);
                // The caller reads back its own descriptor, and the rest.
                assert_eq!(le32(mem(&o, ARG), 48), fd as u32);
                assert_eq!(le32(mem(&o, ARG), 0), 0x1234);
            }
            None => assert!(sends(&o).is_empty()),
        }
    }
}

fn control(ctl: u32, nested: Vec<u8>, w: &mut World) -> Call {
    let mut p = vec![0u8; 32];
    put(&mut p, 8, u64::from(ctl), 4);
    put(&mut p, 16, NESTED, 8);
    put(&mut p, 24, nested.len() as u64, 4);
    put(&mut p, 28, 0xdead, 4);
    w.mem.insert(ARG, p);
    w.mem.insert(NESTED, nested);
    Call::Fd {
        cmd: ioc(3, b'F', 0x2a, 32),
        arg: ARG,
    }
}

#[test]
fn get_build_version_is_answered_here() {
    // Size query: every pointer NULL.
    let mut w = world();
    let call = control(0x101, vec![0u8; 40], &mut w);
    let o = run(dev(0, vec![]), w, call);
    assert_eq!(o.ret, 0);
    assert!(sends(&o).is_empty());
    assert_eq!(le32(mem(&o, NESTED), 0), 31); // the title, NUL included
    assert_eq!(le32(mem(&o, ARG), 28), 0); // NV_OK

    // The strings.
    let mut w = world();
    let mut n = vec![0u8; 40];
    put(&mut n, 0, 64, 4);
    put(&mut n, 8, A, 8);
    put(&mut n, 16, B, 8);
    put(&mut n, 24, C, 8);
    put(&mut n, 32, u64::MAX, 8);
    for at in [A, B, C] {
        w.mem.insert(at, vec![0xee; 64]);
    }
    let call = control(0x101, n, &mut w);
    let o = run(dev(0, vec![]), w, call);
    assert_eq!(o.ret, 0);
    assert_eq!(&mem(&o, A)[..10], b"610.57.04\0");
    assert_eq!(&mem(&o, B)[..10], b"610.57.04\0");
    assert_eq!(&mem(&o, C)[..31], b"NVIDIA UNIX Open Kernel Module\0");
    assert_eq!(mem(&o, A)[10], 0xee);
    assert_eq!(&mem(&o, NESTED)[32..40], &[0; 8]);

    // Too small for them.
    let mut w = world();
    let mut n = vec![0u8; 40];
    put(&mut n, 0, 16, 4);
    put(&mut n, 8, A, 8);
    w.mem.insert(A, vec![0; 64]);
    let call = control(0x101, n, &mut w);
    assert_eq!(run(dev(0, vec![]), w, call).ret, -22);
}

#[test]
fn time_correlation_refuses_the_tsc_and_rebases_the_others() {
    let mut w = world();
    let call = control(
        0x2080_0406,
        vec![0x02, 1, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8],
        &mut w,
    );
    let o = run(dev(0, vec![]), w, call);
    assert_eq!(o.ret, 0);
    assert!(sends(&o).is_empty());
    assert_eq!(le32(mem(&o, ARG), 28), 0x56); // NV_ERR_NOT_SUPPORTED

    // The block is read whole, once, before the clock is looked at: one that
    // does not all read is -EFAULT, in both (the C once read the clock byte
    // alone and answered NOT_SUPPORTED).
    let mut w = world();
    let call = control(0x2080_0406, vec![0x02, 1, 0, 0, 0, 0, 0, 0], &mut w);
    let mut p = w.mem[&ARG].clone();
    put(&mut p, 24, 4096, 4);
    w.mem.insert(ARG, p);
    let o = run(dev(0, vec![]), w, call);
    assert_eq!(o.ret, -14);
    assert!(sends(&o).is_empty());

    // OSTIME, microseconds of realtime: moved by the clock offset (1 ms).
    let mut w = world();
    let mut n = vec![0u8; 8 + 2 * 16];
    n[0] = 0x01;
    n[1] = 2;
    put(&mut n, 8, 5_000_000, 8);
    put(&mut n, 24, 7_000_000, 8);
    let call = control(0x2080_0406, n.clone(), &mut w);
    let mut params = w.mem[&ARG].clone();
    put(&mut params, 28, 0, 4);
    w.canned = vec![reply(0, &params, &n, &[])];
    let o = run(dev(0, vec![]), w, call);
    assert_eq!(o.ret, 0);
    let back = mem(&o, NESTED);
    assert_eq!(
        u64::from_le_bytes(back[8..16].try_into().unwrap()),
        5_001_000
    );
    assert_eq!(
        u64::from_le_bytes(back[24..32].try_into().unwrap()),
        7_001_000
    );
}

#[test]
fn os_unix_descriptors_are_ours_or_refused() {
    let mut w = world();
    let mut n = vec![0u8; 24];
    put(&mut n, 16, 3, 4);
    let call = control(0x3d05, n.clone(), &mut w);
    let params = w.mem[&ARG].clone();
    let mut echo = n.clone();
    put(&mut echo, 16, 101, 4);
    w.canned = vec![reply(0, &params, &echo, &[])];
    let o = run(dev(0, vec![]), w, call);
    assert_eq!(o.ret, 0);
    assert_eq!(le32(&payload(&sends(&o)[0])[32..], 16), 101);
    assert_eq!(le32(mem(&o, NESTED), 16), 3);

    let mut w = world();
    put(&mut n, 16, 99, 4);
    let call = control(0x3d05, n, &mut w);
    let o = run(dev(0, vec![]), w, call);
    assert_eq!(o.ret, -9);
    assert!(sends(&o).is_empty());
    assert!(o
        .world
        .events
        .iter()
        .any(|e| matches!(e, Ev::Warn(s) if s.contains("names fd"))));
}

#[test]
fn fifo_get_channellist_sends_both_lists_and_gets_one_back() {
    let mut w = world();
    let mut n = vec![0u8; 24];
    put(&mut n, 0, 3, 4);
    put(&mut n, 8, A, 8);
    put(&mut n, 16, B, 8);
    w.mem.insert(A, vec![1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
    w.mem.insert(B, vec![0xbb; 12]);
    let call = control(0x0080_170d, n.clone(), &mut w);
    let params = w.mem[&ARG].clone();
    // The backend's deep block, as laid out, with RM's writes.
    let mut deep = Vec::new();
    for v in [2u32, 0, 8, 12, 16, 12] {
        deep.extend_from_slice(&v.to_le_bytes());
    }
    deep.extend_from_slice(&[0x11; 12]);
    deep.extend_from_slice(&[0x22; 12]);
    w.canned = vec![reply(0, &params, &n, &deep)];
    let o = run(dev(BCAP_DEEP_SEGS, vec![]), w, call);
    assert_eq!(o.ret, 0);
    let s = &sends(&o)[0];
    assert_eq!(le32(s, 32), 0xffff_ffff); // NVGPU_DEEP_SEGMENTED
    assert_eq!(le32(s, 36), 8 + 16 + 24);
    let d = &payload(s)[32 + 24..];
    assert_eq!(&d[..24], &deep[..24]);
    assert_eq!(&d[24..36], &[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
    assert_eq!(&d[36..48], &[0xbb; 12]);
    // RM only reads the handles; the channel list comes back.
    assert_eq!(mem(&o, A), &[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
    assert_eq!(mem(&o, B), &[0x22; 12]);

    // Without deep segments, one list goes as a V1V2-style pointer: none
    // (no entry), and the backend zeroes the pointers.
    let mut w = world();
    w.mem.insert(A, vec![0; 12]);
    w.mem.insert(B, vec![0; 12]);
    let call = control(0x0080_170d, n, &mut w);
    let o = run(dev(0, vec![]), w, call);
    let s = &sends(&o)[0];
    assert_eq!((le32(s, 32), le32(s, 36)), (0, 0));
}

#[test]
fn an_event_allocation_names_its_file_by_handle() {
    let mut w = world();
    let mut n = vec![0u8; 24];
    put(&mut n, 16, 4, 4);
    let mut p = vec![0u8; 48];
    put(&mut p, 12, 0x79, 4);
    put(&mut p, 16, NESTED, 8);
    w.mem.insert(ARG, p.clone());
    w.mem.insert(NESTED, n.clone());
    let mut echo = n.clone();
    put(&mut echo, 16, 102, 4);
    w.canned = vec![reply(0, &p, &echo, &[])];
    let o = run(
        dev(BCAP_PROC_ID, vec![]),
        w,
        Call::Fd {
            cmd: ioc(3, b'F', 0x2b, 48),
            arg: ARG,
        },
    );
    assert_eq!(o.ret, 0);
    let s = &sends(&o)[0];
    assert_eq!(le32(&payload(s)[48..], 16), 102);
    // The calling process after the blocks.
    assert_eq!(s.len(), 40 + 48 + 24 + 16);
    assert_eq!(le32(mem(&o, NESTED), 16), 4);

    // paramsSize 0: sized from the class (NV01_EVENT_OS_EVENT: 24).
    let mut w = world();
    put(&mut p, 32, 0, 4);
    w.mem.insert(ARG, p);
    w.mem.insert(NESTED, n);
    let o = run(
        dev(0, vec![]),
        w,
        Call::Fd {
            cmd: ioc(3, b'F', 0x2b, 48),
            arg: ARG,
        },
    );
    assert_eq!(le32(&sends(&o)[0], 28), 24);
}

#[test]
fn idle_channels_sends_a_list_with_its_arrays() {
    let mut w = world();
    let mut p = vec![0u8; 56];
    put(&mut p, 12, 3, 4);
    put(&mut p, 16, A, 8);
    put(&mut p, 24, B, 8);
    put(&mut p, 32, C, 8);
    for (at, v) in [(A, 0xaa), (B, 0xbb), (C, 0xcc)] {
        w.mem.insert(at, vec![v; 12]);
    }
    w.mem.insert(ARG, p.clone());
    let cmd = ioc(3, b'F', 0x41, 56);
    let o = run(
        dev(BCAP_DEEP_SEGS, vec![]),
        w.clone(),
        Call::Fd { cmd, arg: ARG },
    );
    let s = &sends(&o)[0];
    assert_eq!((le32(s, 32), le32(s, 36)), (0xffff_ffff, 8 + 24 + 36));
    assert_eq!(&payload(s)[56 + 32..56 + 44], &[0xaa; 12]);

    // The one-channel form goes flat.
    put(&mut p, 40, 0x10, 4);
    w.mem.insert(ARG, p);
    let o = run(dev(BCAP_DEEP_SEGS, vec![]), w, Call::Fd { cmd, arg: ARG });
    let s = &sends(&o)[0];
    assert_eq!((le32(s, 32), le32(s, 36), s.len()), (0, 0, 40 + 56));
}

fn alloc_memory(va: u64, limit: u64, w: &mut World) -> Call {
    let mut p = vec![0u8; 56];
    put(&mut p, 12, 0x71, 4);
    put(&mut p, 24, va, 8);
    put(&mut p, 32, limit, 8);
    put(&mut p, 48, u64::from(u32::MAX), 4);
    w.mem.insert(ARG, p);
    Call::Fd {
        cmd: ioc(3, b'F', 0x27, 56),
        arg: ARG,
    }
}

#[test]
fn memory_the_caller_has_is_registered_by_its_pages() {
    let d = dev(BCAP_OS_DESC, vec![(0x27, 48)]);
    let va = 0x7f12_3456_7100;
    let mut w = world();
    let call = alloc_memory(va, 0x2fff, &mut w);
    let mut back = w.mem[&ARG].clone();
    put(&mut back, 40, 0, 4);
    put(&mut back, 0, 0x55, 4);
    w.canned = vec![reply(0, &back, &[], &77u64.to_le_bytes())];
    let o = run(d.clone(), w, call);
    assert_eq!(o.ret, 0);
    let ev = &o.world.events;
    assert_eq!(ev[0], Ev::Reap);
    assert_eq!(
        ev[1],
        Ev::Pin {
            start: va & !0xfff,
            n: 4,
            write: true
        }
    );
    assert!(matches!(
        ev.last(),
        Some(Ev::Keep {
            id: 77,
            n: 4,
            write: true
        })
    ));
    let s = &sends(&o)[0];
    assert_eq!(le32(s, 32), 0xffff_fffe); // NVGPU_DEEP_PAGE_LIST
    let h = &payload(s)[56..];
    let nruns = le32(h, 0) as usize;
    assert_eq!(le32(s, 36) as usize, 8 + 16 * nruns);
    let pages: u32 = (0..nruns).map(|i| le32(h, 8 + 16 * i + 8)).sum();
    assert_eq!(pages, 4);
    assert_eq!(le32(mem(&o, ARG), 0), 0x55);

    // Pages that would not pin: RM's own answer, in a call that succeeded.
    let mut w = world();
    w.pin_fails = true;
    let call = alloc_memory(va, 0x2fff, &mut w);
    let o = run(d.clone(), w, call);
    assert_eq!(o.ret, 0);
    assert!(sends(&o).is_empty());
    assert_eq!(le32(mem(&o, ARG), 40), 0x1e);

    // A registration whose caller gave up after it went out: the pins go
    // with the request, for its late reply to settle (nvgpu_osdesc_late()),
    // neither kept under no id until remove() nor unpinned under RM.
    for err in [-4, -110] {
        let mut w = world();
        w.fail = vec![err];
        let call = alloc_memory(va, 0x2fff, &mut w);
        let o = run(d.clone(), w, call);
        assert_eq!(o.ret, i64::from(err));
        let ev = &o.world.events;
        assert!(
            matches!(ev.last(), Some(Ev::HandOver { n: 4, write: true })),
            "{ev:?}"
        );
        assert!(!ev
            .iter()
            .any(|e| matches!(e, Ev::Keep { .. } | Ev::Unpin { .. })));
    }

    // A range within a page of 2^64 is not zero pages: it is not ours at
    // all, and goes the usual way (the C registered it with no pages).
    let mut w = world();
    let call = alloc_memory(0, u64::MAX - 1, &mut w);
    let o = run(d, w, call);
    assert!(!o.world.events.iter().any(|e| matches!(e, Ev::Pin { .. })));
    assert_eq!(le32(&sends(&o)[0], 32), 0);
}

/// An IOCTL2 reply: the OUT buffers' bytes, no records.
fn i2_reply(ret: i32, nbuf: u32, data: &[u8]) -> Vec<u8> {
    let mut r = Vec::new();
    for v in [
        10u32,
        0,
        0,
        0,
        ret as u32,
        nbuf,
        0,
        0,
        data.len() as u32,
        0,
        0,
        0,
    ] {
        r.extend_from_slice(&v.to_le_bytes());
    }
    r.extend_from_slice(data);
    r
}

#[test]
fn getresources_copies_back_what_the_kernel_would() {
    // DRM_IOCTL_MODE_GETRESOURCES: four lists, each copied back only as far
    // as the kernel filled it -- min(the count sent, the count now).
    let cmd = 0xc040_64a0;
    let mut w = world();
    w.hooks.mask = 0;
    let mut arg = vec![0u8; 64];
    put(&mut arg, 0, A, 8);
    put(&mut arg, 32, 3, 4);
    w.mem.insert(ARG, arg.clone());
    w.mem.insert(A, vec![0xee; 12]);
    let mut back = arg.clone();
    put(&mut back, 0, 0, 8);
    put(&mut back, 32, 1, 4);
    put(&mut back, 48, 1920, 4);
    let mut data = back.clone();
    data.extend_from_slice(&[7, 0, 0, 0, 8, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0]);
    w.canned = vec![i2_reply(0, 2, &data)];
    let o = run(
        dev(0, vec![]),
        w,
        Call::I2 {
            sclass: 2,
            cmd,
            uarg: ARG,
            render: 5,
            xflags: 0,
        },
    );
    assert_eq!(o.ret, 0);
    // The request: the argument and the list, sent zeroed (OUT only).
    let s = &sends(&o)[0];
    assert_eq!(le32(s, 16), cmd);
    assert_eq!((le32(s, 24), le32(s, 28)), (2, 0));
    // One entry back, the caller's pointer in place, the kernel's count.
    assert_eq!(
        mem(&o, A),
        &[7, 0, 0, 0, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee]
    );
    assert_eq!(
        u64::from_le_bytes(mem(&o, ARG)[0..8].try_into().unwrap()),
        A
    );
    assert_eq!(le32(mem(&o, ARG), 32), 1);
    assert_eq!(le32(mem(&o, ARG), 48), 1920);
}

/// Both implementations with kmalloc() bytes not zeroed but 0xaa, so a
/// path that sends what it never wrote shows up as a difference.
fn run_dirty(d: DevSpec, w: World, call: Call) -> Outcome {
    unsafe { cabi::harness_set_kmalloc_fill(0xaa) };
    let s = Scenario {
        seed: 0,
        dev: d,
        world: w,
        call,
    };
    let r = scen::diff(&s);
    unsafe { cabi::harness_set_kmalloc_fill(0) };
    r.unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn a_size_with_no_parameters_sends_no_heap() {
    // RM_CONTROL: RM refuses it in the struct (rmapiParamsAcquire), so it is
    // answered here, NV_ERR_INVALID_ARGUMENT; the C sent 64 bytes of heap.
    let mut w = world();
    let mut p = vec![0u8; 32];
    put(&mut p, 8, 0x2080_0101, 4);
    put(&mut p, 24, 64, 4);
    w.mem.insert(ARG, p);
    let o = run_dirty(
        dev(0, vec![]),
        w,
        Call::Fd {
            cmd: ioc(3, b'F', 0x2a, 32),
            arg: ARG,
        },
    );
    assert_eq!(o.ret, 0);
    assert!(sends(&o).is_empty());
    assert_eq!(le32(mem(&o, ARG), 28), 0x1f);

    // RM_ALLOC: RM sizes parameters by the class and takes none with a NULL
    // pointer, so none go and the host is told 0; the caller's comes back.
    let mut w = world();
    let mut p = vec![0u8; 48];
    put(&mut p, 12, 0x80, 4);
    put(&mut p, 32, 64, 4);
    w.mem.insert(ARG, p.clone());
    let mut back = p.clone();
    put(&mut back, 32, 0, 4);
    put(&mut back, 40, 0, 4);
    w.canned = vec![reply(0, &back, &[], &[])];
    let o = run_dirty(
        dev(BCAP_PROC_ID, vec![]),
        w,
        Call::Fd {
            cmd: ioc(3, b'F', 0x2b, 48),
            arg: ARG,
        },
    );
    assert_eq!(o.ret, 0);
    let s = &sends(&o)[0];
    assert_eq!(le32(s, 28), 0); // nested_len
    assert_eq!(le32(payload(s), 32), 0); // paramsSize as sent
    assert_eq!(s.len(), 40 + 48 + 16);
    assert_eq!(le32(mem(&o, ARG), 32), 64);

    // v1 NVKMS: NVKMS fails the copy-in (nvKmsIoctl), -EPERM, nothing sent.
    let mut w = world();
    let mut outer = vec![0u8; 16];
    put(&mut outer, 0, 3, 4);
    put(&mut outer, 4, 24, 4);
    w.mem.insert(ARG, outer);
    let o = run_dirty(
        dev(0, vec![]),
        w,
        Call::Modeset {
            cmd: ioc(3, 0x6d, 0, 16),
            arg: ARG,
        },
    );
    assert_eq!(o.ret, -1);
    assert!(sends(&o).is_empty());
}

#[test]
fn a_v1v2_count_that_wraps_carries_no_deep_block() {
    // GPU_GET_INFO counts entries of 8 bytes: 2^29 + 1 of them wrapped to
    // one entry in u32, and 8 bytes went as the list.
    let mut w = world();
    let mut n = vec![0u8; 16];
    put(&mut n, 0, 0x2000_0001, 4);
    put(&mut n, 8, A, 8);
    w.mem.insert(A, vec![0x77; 64]);
    let call = control(0x2080_0101, n, &mut w);
    let o = run(dev(0, vec![]), w, call);
    let s = &sends(&o)[0];
    assert_eq!((le32(s, 32), le32(s, 36)), (0, 0));
    assert_eq!(s.len(), 40 + 32 + 16);
}

#[test]
fn a_status_that_is_not_an_errno_is_eproto_and_nothing_comes_back() {
    // The backend's status is 0 or a -errno. Anything else was returned from
    // the ioctl as it was -- a positive "result" no native driver gives, or
    // an errno past MAX_ERRNO -- and the flat path copied the block back
    // beside it. Now: -EPROTO, and the caller's memory as it was.
    for bad in [1i32, 5, i32::MAX, -4096, i32::MIN] {
        // A flat escape (nvgpu_ioctl_simple).
        let mut w = world();
        let arg = vec![0x11u8; 24];
        w.mem.insert(ARG, arg.clone());
        w.canned = vec![reply(bad, &[0x99; 24], &[], &[])];
        let o = run(
            dev(0, vec![]),
            w,
            Call::Fd {
                cmd: ioc(3, b'F', 0x50, 24),
                arg: ARG,
            },
        );
        assert_eq!(o.ret, -71, "flat, status {bad}");
        assert_eq!(mem(&o, ARG), &arg[..], "flat, status {bad}");

        // RM_CONTROL, whose struct and nested block come back on any status.
        let mut w = world();
        let call = control(0x2080_0101, vec![0x22; 16], &mut w);
        let params = w.mem[&ARG].clone();
        w.canned = vec![reply(bad, &[0x99; 32], &[0x98; 16], &[])];
        let o = run(dev(0, vec![]), w, call);
        assert_eq!(o.ret, -71, "control, status {bad}");
        assert_eq!(mem(&o, ARG), &params[..]);
        assert_eq!(mem(&o, NESTED), &[0x22; 16][..]);

        // A descriptor at a fixed offset (nvgpu_ioctl_translate_fd).
        let mut w = world();
        let mut arg = vec![0u8; 56];
        put(&mut arg, 48, 3, 4);
        w.mem.insert(ARG, arg.clone());
        w.canned = vec![reply(bad, &[0x99; 56], &[], &[])];
        let o = run(
            dev(0, vec![(0x27, 48)]),
            w,
            Call::Fd {
                cmd: ioc(3, b'F', 0x27, 56),
                arg: ARG,
            },
        );
        assert_eq!(o.ret, -71, "fd, status {bad}");
        assert_eq!(mem(&o, ARG), &arg[..]);

        // A v1 backend's NVKMS command.
        let mut w = world();
        let mut outer = vec![0u8; 16];
        put(&mut outer, 0, 3, 4);
        put(&mut outer, 4, 8, 4);
        put(&mut outer, 8, NESTED, 8);
        w.mem.insert(ARG, outer.clone());
        w.mem.insert(NESTED, vec![0x33; 8]);
        w.canned = vec![reply(bad, &[0x99; 16], &[0x98; 8], &[])];
        let o = run(
            dev(0, vec![]),
            w,
            Call::Modeset {
                cmd: ioc(3, 0x6d, 0, 16),
                arg: ARG,
            },
        );
        assert_eq!(o.ret, -71, "nvkms, status {bad}");
        assert_eq!(mem(&o, ARG), &outer[..]);
        assert_eq!(mem(&o, NESTED), &[0x33; 8][..]);
    }
    // The errnos at either end still pass as they are.
    for good in [-1i32, -4095] {
        let mut w = world();
        w.mem.insert(ARG, vec![0x11u8; 24]);
        w.canned = vec![reply(good, &[], &[], &[])];
        let o = run(
            dev(0, vec![]),
            w,
            Call::Fd {
                cmd: ioc(3, b'F', 0x50, 24),
                arg: ARG,
            },
        );
        assert_eq!(o.ret, i64::from(good));
    }
}

#[test]
fn nvkms_takes_its_one_ioctl_only() {
    // Another size, or another number: -ENOTTY, as nvkms_ioctl, with nothing
    // read or written (the C read and wrote 16 bytes whatever the size).
    for cmd in [ioc(3, 0x6d, 0, 8), ioc(3, 0x6d, 1, 16), ioc(3, 0x6d, 0, 24)] {
        let mut w = world();
        let mut outer = vec![0u8; 16];
        put(&mut outer, 0, 3, 4);
        w.mem.insert(ARG, outer.clone());
        let o = run(dev(0, vec![]), w, Call::Modeset { cmd, arg: ARG });
        assert_eq!(o.ret, -25, "{cmd:#x}");
        assert!(sends(&o).is_empty());
        assert_eq!(mem(&o, ARG), &outer[..]);
    }
}

#[test]
fn a_field_of_a_width_the_generator_refuses_is_refused() {
    // A descriptor 2 bytes wide (the C put 4 bytes back over it), and a GEM
    // handle 2 bytes wide: -EINVAL before anything is sent.
    let mut d = dev(0, vec![]);
    d.bad_schema = true;
    for cmd in [0xc010_64f0u32, 0xc010_64f1] {
        let mut w = world();
        let mut arg = vec![0u8; 16];
        put(&mut arg, 0, 3, 2);
        w.mem.insert(ARG, arg.clone());
        let o = run(
            d.clone(),
            w,
            Call::I2 {
                sclass: 2,
                cmd,
                uarg: ARG,
                render: 5,
                xflags: 0,
            },
        );
        assert_eq!(o.ret, -22, "{cmd:#x}");
        assert!(sends(&o).is_empty());
        assert_eq!(mem(&o, ARG), &arg[..]);
    }
}

#[test]
fn a_failed_gem_proxy_closes_no_handle_another_proxy_owns() {
    // GETFB2 naming host handle 1, then 2, then 1 again; the proxy for 2
    // cannot be made. 2 is closed; 1 is the first proxy's, and stays open
    // (the C closed it too).
    let cmd = 0xc068_64ce;
    let mut w = world();
    w.hooks = Hooks {
        mask: 8,
        seed: 1,
        fail_gem: Some(2),
    };
    let arg = vec![0u8; 104];
    w.mem.insert(ARG, arg.clone());
    let mut r = Vec::new();
    for v in [10u32, 0, 0, 0, 0, 1, 0, 3, 104, 0, 0, 0] {
        r.extend_from_slice(&v.to_le_bytes());
    }
    r.extend_from_slice(&arg);
    for (off, h) in [(20u32, 1u32), (24, 2), (28, 1)] {
        for v in [0u32, off, h, 0] {
            r.extend_from_slice(&v.to_le_bytes());
        }
        r.extend_from_slice(&4096u64.to_le_bytes());
    }
    w.canned = vec![r];
    let o = run(
        dev(0, vec![]),
        w,
        Call::I2 {
            sclass: 2,
            cmd,
            uarg: ARG,
            render: 5,
            xflags: 0,
        },
    );
    assert_eq!(o.ret, -12);
    let closed: Vec<(u32, u32)> = o
        .world
        .events
        .iter()
        .filter_map(|e| {
            if let Ev::GemClose(f, g) = e {
                Some((*f, *g))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(closed, [(5, 2)]);
}

#[test]
fn an_atomic_commit_reserves_its_crtcs_and_bridges_its_fences() {
    use nvgpu_guest_difftest::world::Hook;
    // Two objects: plane 2 (on CRTC 2, per the world) with CRTC_ID = 6 and
    // IN_FENCE_FD = 7; CRTC 3 with OUT_FENCE_PTR. A committing, evented
    // commit with the fence bridge.
    let mut w = world();
    w.hooks = Hooks {
        mask: 1 << 4,
        seed: 1,
        fail_gem: None,
    };
    w.atomic = Some(true);
    w.mem
        .insert(A, [2u32, 3].iter().flat_map(|v| v.to_le_bytes()).collect());
    w.mem
        .insert(B, [2u32, 1].iter().flat_map(|v| v.to_le_bytes()).collect());
    // Property ids by the world's classes: 5 -> CRTC_ID, 6 -> IN_FENCE,
    // 7 -> OUT_PTR.
    w.mem.insert(
        C,
        [5u32, 6, 7].iter().flat_map(|v| v.to_le_bytes()).collect(),
    );
    const V: u64 = 0x7f00_0005_0000;
    const OUT: u64 = 0x7f00_0006_0000;
    w.mem.insert(
        V,
        [6u64, 7, OUT]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
    );
    w.mem.insert(OUT, vec![0; 4]);
    let mut a = vec![0u8; 56];
    put(&mut a, 0, 1, 4);
    put(&mut a, 4, 2, 4);
    put(&mut a, 8, A, 8);
    put(&mut a, 16, B, 8);
    put(&mut a, 24, C, 8);
    put(&mut a, 32, V, 8);
    put(&mut a, 48, 0xabcd, 8);
    w.mem.insert(ARG, a);
    let test_only = {
        let mut w = w.clone();
        let mut a = w.mem[&ARG].clone();
        put(&mut a, 0, 0x100, 4);
        w.mem.insert(ARG, a);
        w
    };
    let o = run(
        dev(0, vec![]),
        w,
        Call::I2 {
            sclass: 2,
            cmd: 0xc038_64bc,
            uarg: ARG,
            render: 5,
            xflags: 0,
        },
    );
    let hooks: Vec<Hook> = o
        .world
        .events
        .iter()
        .filter_map(|e| {
            if let Ev::Hook(h) = e {
                Some(h.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(hooks.contains(&Hook::ALearn { obj: 2, crtc: 6 }));
    // The hook sees a real commit: a TEST_ONLY one's fences are only
    // checked. (The Rust parse said so only once it had finished, and every
    // IN_FENCE_FD went to the host as -1.)
    assert!(hooks.contains(&Hook::AInFence {
        buf: 4,
        off: 8,
        fd: 7,
        commit: true
    }));
    assert!(hooks.contains(&Hook::AOutFence {
        buf: 4,
        off: 16,
        uptr: OUT
    }));
    let reserved: Vec<u32> = hooks
        .iter()
        .filter_map(|h| {
            if let Hook::AReserve { crtc, .. } = h {
                Some(*crtc)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(reserved, [2, 6, 3]);
    assert!(hooks.contains(&Hook::AtomicOut {
        commit: true,
        values_buf: 4
    }));
    assert_eq!(mem(&o, OUT), &(-1i32).to_le_bytes());
    // The request carries the in-fence's record and the out-fence's dyn.
    let s = &sends(&o)[0];
    assert_eq!((le32(s, 28), le32(s, 36)), (1, 1));

    // TEST_ONLY: the hook knows that too.
    let o = run(
        dev(0, vec![]),
        test_only,
        Call::I2 {
            sclass: 2,
            cmd: 0xc038_64bc,
            uarg: ARG,
            render: 5,
            xflags: 0,
        },
    );
    assert!(o.world.events.contains(&Ev::Hook(Hook::AInFence {
        buf: 4,
        off: 8,
        fd: 7,
        commit: false
    })));
}
