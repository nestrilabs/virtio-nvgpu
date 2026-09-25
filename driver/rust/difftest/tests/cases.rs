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
    DevSpec { version: "610.57.04".into(), v2: true, caps, max_req: 1 << 20, max_resp: 1 << 20, fdt, handle: 5 }
}

fn world() -> World {
    World {
        fds: FDS.iter().copied().collect(),
        clock: Some(1_000_000),
        hooks: Hooks { mask: 0x3f, seed: 1 },
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
    let s = Scenario { seed: 0, dev: d, world: w, call };
    scen::diff(&s).unwrap_or_else(|e| panic!("{e}"))
}

fn sends(o: &Outcome) -> Vec<Vec<u8>> {
    o.world.events.iter().filter_map(|e| if let Ev::Send(b) = e { Some(b.clone()) } else { None }).collect()
}

fn mem(o: &Outcome, at: u64) -> &[u8] {
    &o.world.mem[&at]
}

/// A protocol-v1 reply.
fn reply(status: i32, data: &[u8], nested: &[u8], deep: &[u8]) -> Vec<u8> {
    let mut r = Vec::new();
    for v in [3u32, 0, status as u32, 0, data.len() as u32, nested.len() as u32, deep.len() as u32] {
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
    for (fd, sent, ret) in [(3i32, Some(101u32), 0i64), (-1, Some(u32::MAX), 0), (-2, None, -9), (99, None, -9)] {
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
    Call::Fd { cmd: ioc(3, b'F', 0x2a, 32), arg: ARG }
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
    let call = control(0x2080_0406, vec![0x02, 1, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8], &mut w);
    let o = run(dev(0, vec![]), w, call);
    assert_eq!(o.ret, 0);
    assert!(sends(&o).is_empty());
    assert_eq!(le32(mem(&o, ARG), 28), 0x56); // NV_ERR_NOT_SUPPORTED

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
    assert_eq!(u64::from_le_bytes(back[8..16].try_into().unwrap()), 5_001_000);
    assert_eq!(u64::from_le_bytes(back[24..32].try_into().unwrap()), 7_001_000);
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
    assert!(o.world.events.iter().any(|e| matches!(e, Ev::Warn(s) if s.contains("names fd"))));
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
    let o = run(dev(BCAP_PROC_ID, vec![]), w, Call::Fd { cmd: ioc(3, b'F', 0x2b, 48), arg: ARG });
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
    let o = run(dev(0, vec![]), w, Call::Fd { cmd: ioc(3, b'F', 0x2b, 48), arg: ARG });
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
    let o = run(dev(BCAP_DEEP_SEGS, vec![]), w.clone(), Call::Fd { cmd, arg: ARG });
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
    Call::Fd { cmd: ioc(3, b'F', 0x27, 56), arg: ARG }
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
    assert_eq!(ev[1], Ev::Pin { start: va & !0xfff, n: 4, write: true });
    assert!(matches!(ev.last(), Some(Ev::Keep { id: 77, n: 4, write: true })));
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
    for v in [10u32, 0, 0, 0, ret as u32, nbuf, 0, 0, data.len() as u32, 0, 0, 0] {
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
    let o = run(dev(0, vec![]), w, Call::I2 { sclass: 2, cmd, uarg: ARG, render: 5, xflags: 0 });
    assert_eq!(o.ret, 0);
    // The request: the argument and the list, sent zeroed (OUT only).
    let s = &sends(&o)[0];
    assert_eq!(le32(s, 16), cmd);
    assert_eq!((le32(s, 24), le32(s, 28)), (2, 0));
    // One entry back, the caller's pointer in place, the kernel's count.
    assert_eq!(mem(&o, A), &[7, 0, 0, 0, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee]);
    assert_eq!(u64::from_le_bytes(mem(&o, ARG)[0..8].try_into().unwrap()), A);
    assert_eq!(le32(mem(&o, ARG), 32), 1);
    assert_eq!(le32(mem(&o, ARG), 48), 1920);
}

#[test]
fn the_c_sends_heap_bytes_it_never_wrote() {
    // RM_CONTROL with paramsSize set and params NULL: the C allocates the
    // request with kmalloc() and sends paramsSize bytes it never filled --
    // guest kernel heap, to the backend. The Rust sends zeroes. (With
    // kmalloc() zeroed, as the other tests have it, the two agree.)
    let mut w = world();
    let mut p = vec![0u8; 32];
    put(&mut p, 8, 0x2080_0101, 4);
    put(&mut p, 24, 64, 4);
    w.mem.insert(ARG, p);
    let s = Scenario { seed: 0, dev: dev(0, vec![]), world: w.clone(), call: Call::Fd { cmd: ioc(3, b'F', 0x2a, 32), arg: ARG } };
    let r = scen::run_rust(&s);
    unsafe { cabi::harness_set_kmalloc_fill(0xaa) };
    let c = scen::run_c(&s, w);
    unsafe { cabi::harness_set_kmalloc_fill(0) };
    let (cs, rs) = (&sends(&c)[0], &sends(&r)[0]);
    assert_eq!(cs.len(), 40 + 32 + 64);
    assert_eq!(&cs[72..], &[0xaa; 64][..]);
    assert_eq!(&rs[72..], &[0; 64][..]);
}
