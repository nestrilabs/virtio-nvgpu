// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use super::*;
use abi::ioctl::*;
use std::cell::RefCell;

/// A call RM saw: what it was (control or class), and the parameters'
/// words at 0 and 8 as RM read them, each with whether it is an address
/// in the call's own memory.
type Seen = (u32, [(u64, bool); 2]);

std::thread_local! {
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
}

fn seen() -> Vec<Seen> {
    SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// RM: note the first two words of the parameters as the host reads
/// them, answer NV_OK.
fn fake_rm(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
    let (b, others) = arg.split();
    let escape = (request & 0xff) as u32;
    let (what, status) = match escape {
        NV_ESC_RM_CONTROL => (u32::from_le_bytes(b[8..12].try_into().unwrap()), 28),
        NV_ESC_RM_ALLOC => (u32::from_le_bytes(b[12..16].try_into().unwrap()), 40),
        _ => return 0,
    };
    let p = u64::from_le_bytes(b[16..24].try_into().unwrap());
    let word = |at: u64| {
        let v = others.peek(p + at, 8);
        (v, others.reach(v, 1).is_some())
    };
    SEEN.with(|s| s.borrow_mut().push((what, [word(0), word(8)])));
    b[status..status + 4].fill(0);
    0
}

fn backend() -> (NvidiaBackend, u32) {
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_rm);
    // This path's own rule: the allowlist in front of it is tested
    // with rmallow.rs.
    be.set_rm_allowlist(crate::rmallow::Mode::Log);
    let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
    let ctl = be.adopt_for_test(null, HandleKind::Dev(DeviceKind::Ctl));
    (be, ctl)
}

/// A v1 call: `outer`, its `nested` parameters, and a deep block the
/// guest says the pointer at `deep_at` of them addresses. The reply's
/// status, and what follows the IoctlResp.
fn call(
    be: &mut NvidiaBackend,
    on: u32,
    cmd: u32,
    outer: &[u8],
    nested: &[u8],
    deep: (u32, &[u8]),
) -> (i32, Vec<u8>) {
    let mut req = vec![0u8; size_of::<MsgHeader>()];
    write_struct(
        &mut req,
        &MsgHeader {
            msg_type: MsgType::Ioctl as u32,
            handle: on,
            status: 0,
            req_id: 0,
        },
    );
    let at = req.len();
    req.resize(at + size_of::<IoctlReq>(), 0);
    write_struct(
        &mut req[at..],
        &IoctlReq {
            cmd,
            data_len: outer.len() as u32,
            nested_offset: outer.len() as u32,
            nested_len: nested.len() as u32,
            deep_ptr_offset: deep.0,
            deep_len: deep.1.len() as u32,
        },
    );
    req.extend_from_slice(outer);
    req.extend_from_slice(nested);
    req.extend_from_slice(deep.1);
    let mut resp = vec![0u8; 8192];
    let n = be.dispatch(&req, &mut resp);
    let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
    let st = read_struct::<MsgHeader>(&resp, 0).status;
    (st, resp[body.min(n)..n].to_vec())
}

fn control(cmd: u32, size: u32) -> [u8; 32] {
    let mut o = [0u8; 32];
    o[8..12].copy_from_slice(&cmd.to_le_bytes());
    o[16..24].copy_from_slice(&0x7000u64.to_le_bytes());
    o[24..28].copy_from_slice(&size.to_le_bytes());
    o
}

const GUEST_PTR: u64 = 0x7fff_1234_5678;

/// SET_ZBC_COLOR_CLEAR keeps no pointer: an address written into its
/// color words went to RM as a clear color, into the GPU-wide ZBC
/// table any tenant reads -- an address of this process.
#[test]
fn a_deep_block_puts_no_address_where_rm_follows_no_pointer() {
    const SET_ZBC_COLOR_CLEAR: u32 = 0x9096_0101;
    assert!(crate::guestptr::control_pointers(SET_ZBC_COLOR_CLEAR).is_empty());
    let (mut be, ctl) = backend();
    let mut nested = [0u8; 44];
    nested[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
    let deep = [0xabu8; 16];
    let cmd = _IOWR(NV_ESC_RM_CONTROL, 32) as u32;
    let outer = control(SET_ZBC_COLOR_CLEAR, 44);
    let (st, back) = call(&mut be, ctl, cmd, &outer, &nested, (8, &deep));
    assert_eq!(st, 0);
    let s = seen();
    assert_eq!(s.len(), 1);
    assert_eq!(
        s[0].1[1],
        (GUEST_PTR, false),
        "RM reads the guest's own bytes there, never an address of ours"
    );
    // The caller's parameters and block come back as sent.
    assert_eq!(&back[32..32 + 44], &nested);
    assert_eq!(&back[32 + 44..], &deep);

    // Where RM does follow one (GET_SURFACE_INFO's list), it is
    // relocated as before.
    const GET_SURFACE_INFO: u32 = 0x0041_0110;
    assert_eq!(crate::guestptr::control_pointers(GET_SURFACE_INFO), &[8]);
    let mut nested = [0u8; 16];
    nested[0..4].copy_from_slice(&2u32.to_le_bytes());
    nested[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
    let outer = control(GET_SURFACE_INFO, 16);
    let (st, back) = call(&mut be, ctl, cmd, &outer, &nested, (8, &deep));
    assert_eq!(st, 0);
    let s = seen();
    assert!(s[0].1[1].1, "the list: a block of the call");
    assert_eq!(&back[32 + 8..32 + 16], &GUEST_PTR.to_le_bytes());
}

/// No RM_ALLOC class takes a deep block (classes whose parameters hold
/// pointers are refused, guestptr.rs): one sent is refused, and RM is
/// not asked.
#[test]
fn an_allocation_takes_no_deep_block() {
    let (mut be, ctl) = backend();
    let mut outer = [0u8; 48];
    outer[0..4].copy_from_slice(&0xc1d0_0001u32.to_le_bytes());
    outer[12..16].copy_from_slice(&0x0080u32.to_le_bytes());
    outer[16..24].copy_from_slice(&0x7000u64.to_le_bytes());
    outer[32..36].copy_from_slice(&56u32.to_le_bytes());
    let nested = [0u8; 56];
    let cmd = _IOWR(NV_ESC_RM_ALLOC, 48) as u32;
    let (st, _) = call(&mut be, ctl, cmd, &outer, &nested, (8, &[1u8; 8]));
    assert_eq!(st, -libc::EINVAL);
    assert!(seen().is_empty());
    let (st, _) = call(&mut be, ctl, cmd, &outer, &nested, (0, &[]));
    assert_eq!(st, 0, "without one it goes");
    assert_eq!(seen().len(), 1);
}
