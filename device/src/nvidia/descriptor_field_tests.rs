// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use super::*;
use abi::ioctl::*;
use std::cell::RefCell;
use std::os::fd::AsRawFd;

std::thread_local! {
    /// (control, the descriptor RM was handed) for each RM_CONTROL.
    static SEEN: RefCell<Vec<(u32, i32)>> = const { RefCell::new(Vec::new()) };
}

fn seen() -> Vec<(u32, i32)> {
    SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// RM_CONTROL: record the descriptor field of an OS_UNIX control,
/// answer NV_OK.
fn fake_rm(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
    let len = hostfd::ioc_size(request as u32);
    let (b, others) = arg.split();
    let b = &mut b[..len];
    if (request & 0xff) as u32 == NV_ESC_RM_CONTROL {
        let cmd = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let p = u64::from_le_bytes(b[16..24].try_into().unwrap());
        if let Some(crate::rmctl::UnixCtl::Fd { at }) = crate::rmctl::unix_control(cmd) {
            // The backend pointed pParams at its own copy of the
            // parameters, which hold the field (checked before the call).
            let fd = others.peek(p + at as u64, 4) as u32 as i32;
            SEEN.with(|s| s.borrow_mut().push((cmd, fd)));
        } else {
            SEEN.with(|s| s.borrow_mut().push((cmd, i32::MIN)));
        }
        b[28..32].copy_from_slice(&0u32.to_le_bytes());
    } else if (request & 0xff) as u32 == NV_ESC_RM_ALLOC {
        // (class | 1 << 31, 0): an allocation RM was asked for.
        let class = u32::from_le_bytes(b[12..16].try_into().unwrap());
        SEEN.with(|s| s.borrow_mut().push((class | 1 << 31, 0)));
    }
    0
}

fn devnull() -> OwnedFd {
    std::fs::File::open("/dev/null").unwrap().into()
}

fn control(be: &mut NvidiaBackend, on: u32, cmd: u32, params: &[u8]) -> (i32, Vec<u8>) {
    let mut outer = [0u8; 32];
    outer[8..12].copy_from_slice(&cmd.to_le_bytes());
    outer[24..28].copy_from_slice(&(params.len() as u32).to_le_bytes());
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
            cmd: _IOWR(NV_ESC_RM_CONTROL, 32) as u32,
            data_len: 32,
            nested_offset: 32,
            nested_len: params.len() as u32,
            deep_ptr_offset: 0,
            deep_len: 0,
        },
    );
    req.extend_from_slice(&outer);
    req.extend_from_slice(params);
    let mut resp = vec![0u8; 8192];
    let n = be.dispatch(&req, &mut resp);
    let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
    let st = read_struct::<MsgHeader>(&resp, 0).status;
    (st, resp[body.min(n)..n].to_vec())
}

/// NV_ESC_RM_ALLOC (NVOS64) of `class` with `params`.
fn alloc(be: &mut NvidiaBackend, on: u32, class: u32, params: &[u8]) -> (i32, Vec<u8>) {
    let mut outer = [0u8; 48];
    outer[0..4].copy_from_slice(&0xc1d0_0001u32.to_le_bytes());
    outer[4..8].copy_from_slice(&0xc1d0_0001u32.to_le_bytes());
    outer[8..12].copy_from_slice(&0x5000_0001u32.to_le_bytes());
    outer[12..16].copy_from_slice(&class.to_le_bytes());
    outer[32..36].copy_from_slice(&(params.len() as u32).to_le_bytes());
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
            cmd: _IOWR(NV_ESC_RM_ALLOC, 48) as u32,
            data_len: 48,
            nested_offset: 16,
            nested_len: params.len() as u32,
            deep_ptr_offset: 0,
            deep_len: 0,
        },
    );
    req.extend_from_slice(&outer);
    req.extend_from_slice(params);
    let mut resp = vec![0u8; 8192];
    let n = be.dispatch(&req, &mut resp);
    let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
    let st = read_struct::<MsgHeader>(&resp, 0).status;
    (st, resp[body.min(n)..n].to_vec())
}

fn status_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The RM allowlist (rmallow.rs) is on by default, in front of RM, for
/// controls and classes alike, and the host's release picks its list.
#[test]
fn rm_calls_the_allowlist_lacks_never_reach_rm() {
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_rm);
    be.set_host_driver_version("610.57.04");
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let _ = seen();
    // GPU_SET_POWER and EXEC_REG_OPS: RM lets any user process call
    // them; no workload the project runs does.
    for (cmd, size) in [
        (0x2080_0112u32, 4usize),
        (0x2080_0122, 16),
        (0xdead_beef, 8),
    ] {
        let (st, back) = control(&mut be, ctl, cmd, &vec![0u8; size]);
        assert_eq!(st, 0, "{cmd:#x}: RM's own answer, not a failed ioctl");
        assert_eq!(
            status_at(&back, 28),
            crate::nvos::NV_ERR_NOT_SUPPORTED,
            "{cmd:#x}"
        );
    }
    assert!(seen().is_empty(), "none reached RM");
    // GPU_GET_INFO_V2 is 580 bytes in 610.57.04: another size is RM's
    // INVALID_PARAM_STRUCT, and the right one goes.
    let (_, back) = control(&mut be, ctl, 0x2080_0102, &[0u8; 64]);
    assert_eq!(
        status_at(&back, 28),
        crate::nvos::NV_ERR_INVALID_PARAM_STRUCT
    );
    assert!(seen().is_empty());
    let (st, _) = control(&mut be, ctl, 0x2080_0102, &[0u8; 580]);
    assert_eq!(st, 0);
    assert_eq!(seen(), [(0x2080_0102, i32::MIN)]);
    // A class: NV40_I2C is RM's to refuse a user anyway, NV20_SUBDEVICE_DIAG
    // is not; neither is the guest's. NV01_ROOT_CLIENT is.
    for class in [0x402cu32, 0x208f] {
        let (st, back) = alloc(&mut be, ctl, class, &[]);
        assert_eq!(st, 0, "{class:#x}");
        assert_eq!(
            status_at(&back, 40),
            crate::nvos::NV_ERR_INVALID_CLASS,
            "{class:#x}"
        );
    }
    assert!(seen().is_empty());
    let (st, _) = alloc(&mut be, ctl, 0x41, &[]);
    assert_eq!(st, 0);
    assert_eq!(seen(), [(0x41 | 1 << 31, 0)]);
}

#[test]
fn in_log_mode_the_allowlist_only_says_what_it_would_refuse() {
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_rm);
    be.set_host_driver_version("610.57.04");
    be.set_rm_allowlist(crate::rmallow::Mode::Log);
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let _ = seen();
    let (st, _) = control(&mut be, ctl, 0x2080_0112, &[0u8; 4]);
    assert_eq!(st, 0);
    assert_eq!(seen(), [(0x2080_0112, i32::MIN)]);
    // And the ABI policy does not reach it: permissive or not, enforced.
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_rm);
    be.set_host_driver_version("610.57.04");
    be.set_abi_policy(AbiPolicy::Permissive);
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let (_, back) = control(&mut be, ctl, 0x2080_0112, &[0u8; 4]);
    assert_eq!(status_at(&back, 28), crate::nvos::NV_ERR_NOT_SUPPORTED);
    assert!(seen().is_empty());
}

fn with_fd(len: usize, at: usize, v: i32) -> Vec<u8> {
    let mut p = vec![0u8; len];
    p[at..at + 4].copy_from_slice(&v.to_le_bytes());
    p
}

#[test]
fn each_export_and_import_control_gets_our_descriptor_of_the_callers_control_file() {
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_rm);
    let ctl_fd = devnull();
    let raw = ctl_fd.as_raw_fd();
    let ctl = be.adopt_for_test(ctl_fd, HandleKind::Dev(DeviceKind::Ctl));
    let _ = seen();
    // (control, parameter size in 610.57.04, where the descriptor is)
    for (cmd, len, at) in [
        (0x3d05u32, 24usize, 16usize),
        (0x3d06, 20, 0),
        (0x3d08, 80, 0),
        (0x3d0a, 76, 72),
        (0x3d0b, 2128, 0),
        (0x3d0c, 648, 0),
    ] {
        let (st, back) = control(&mut be, ctl, cmd, &with_fd(len, at, ctl as i32));
        assert_eq!(st, 0, "{cmd:#x}");
        assert_eq!(seen(), [(cmd, raw)], "{cmd:#x}: RM sees our descriptor");
        let guest = i32::from_le_bytes(back[32 + at..32 + at + 4].try_into().unwrap());
        assert_eq!(guest, ctl as i32, "{cmd:#x}: the guest's handle comes back");
    }
}

#[test]
fn a_number_that_is_no_control_file_of_the_vm_never_reaches_rm() {
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_rm);
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let gpu = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Gpu(0)));
    let ev = be.adopt_for_test(devnull(), HandleKind::Eventfd);
    let _ = seen();
    // Another process's number as the guest sent it, a GPU file, an
    // eventfd, and a negative number other than -1: EBADF, RM not asked.
    for v in [ctl as i32 + 7, gpu as i32, ev as i32, -2, i32::MIN] {
        for (cmd, len, at) in [(0x3d0c, 648, 0), (0x3d06, 20, 0), (0x3d0b, 2128, 0)] {
            let (st, _) = control(&mut be, ctl, cmd, &with_fd(len, at, v));
            assert_eq!(st, -libc::EBADF, "{cmd:#x} naming {v}");
        }
    }
    assert!(seen().is_empty());
    // -1 goes as it is: RM refuses it itself.
    let (st, _) = control(&mut be, ctl, 0x3d0c, &with_fd(648, 0, -1));
    assert_eq!(st, 0);
    assert_eq!(seen(), [(0x3d0c, -1)]);
    // A block too short to hold the field is refused, not forwarded.
    let (st, _) = control(&mut be, ctl, 0x3d0a, &[0u8; 40]);
    assert_eq!(st, -libc::EINVAL);
    assert!(seen().is_empty());
}

#[test]
fn memacct_and_undefined_os_unix_controls_are_answered_without_rm() {
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_rm);
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let _ = seen();
    for cmd in [0x3d0du32, 0x3d0e, 0x3d04, 0x3d20] {
        let (st, back) = control(&mut be, ctl, cmd, &[0u8; 32]);
        assert_eq!(st, 0, "{cmd:#x}: RM's own answer, not a failed ioctl");
        assert_eq!(
            u32::from_le_bytes(back[28..32].try_into().unwrap()),
            crate::nvos::NV_ERR_NOT_SUPPORTED
        );
    }
    assert!(seen().is_empty());
    // FLUSH_USER_CACHE carries no descriptor and goes to RM.
    // OS_GET_GPU_INFO, which RM's tables do not export, is answered as
    // RM answers it, by the allowlist (rmallow.rs).
    let (st, _) = control(&mut be, ctl, 0x3d02, &[0u8; 40]);
    assert_eq!(st, 0);
    assert_eq!(seen().len(), 1);
    let (st, back) = control(&mut be, ctl, 0x3d07, &[0u8; 8]);
    assert_eq!(st, 0);
    assert_eq!(
        u32::from_le_bytes(back[28..32].try_into().unwrap()),
        crate::nvos::NV_ERR_NOT_SUPPORTED
    );
    assert!(seen().is_empty());
}

/// An event's parameters too short to hold its descriptor are refused,
/// not sent for RM to read the field from past them (review 2026-09-29
/// 1.17).
#[test]
fn an_event_block_too_short_for_its_descriptor_never_reaches_rm() {
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_rm);
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    for class in [0x05, 0x79] {
        for len in [4, 16, 19] {
            let (st, _) = alloc(&mut be, ctl, class, &vec![0u8; len]);
            assert_eq!(st, -libc::EINVAL, "class {class:#x}, {len} bytes");
        }
    }
    assert!(seen().is_empty());
    let mut p = [0u8; 24];
    p[16..20].copy_from_slice(&(-1i32).to_le_bytes());
    assert_eq!(alloc(&mut be, ctl, 0x05, &p).0, 0, "-1 passes");
}

#[test]
fn an_fd_carrying_escape_forwards_minus_one_and_refuses_other_negatives() {
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_rm);
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    // NV_ESC_RM_ALLOC_MEMORY: the descriptor at 48 of 56 bytes.
    let send = |be: &mut NvidiaBackend, v: i32| {
        let mut p = vec![0u8; 56];
        p[48..52].copy_from_slice(&v.to_le_bytes());
        let mut req = vec![0u8; size_of::<MsgHeader>()];
        write_struct(
            &mut req,
            &MsgHeader {
                msg_type: MsgType::Ioctl as u32,
                handle: ctl,
                status: 0,
                req_id: 0,
            },
        );
        let at = req.len();
        req.resize(at + size_of::<IoctlReq>(), 0);
        write_struct(
            &mut req[at..],
            &IoctlReq {
                cmd: _IOWR(NV_ESC_RM_ALLOC_MEMORY, 56) as u32,
                data_len: 56,
                ..Default::default()
            },
        );
        req.extend_from_slice(&p);
        let mut resp = vec![0u8; 4096];
        be.dispatch(&req, &mut resp);
        read_struct::<MsgHeader>(&resp, 0).status
    };
    assert_eq!(send(&mut be, -1), 0);
    assert_eq!(send(&mut be, -2), -libc::EBADF);
    assert_eq!(send(&mut be, i32::MIN), -libc::EBADF);
}
