// SPDX-License-Identifier: Apache-2.0
//! The registry's rules, INJECT_OPEN through the dispatcher, and the socket.

#![forbid(unsafe_code)]

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use protocol::inject::{INJ_OP_IMPORT, INJ_VERSION, InjImport, InjReply};

use super::check::{MOD_INVALID, fourcc};
use super::fake::{FakeHost, Obj};
use super::*;
use crate::hostfd;
use crate::quota::Owner;
use protocol::inject::{INJ_REPLY_SIZE, InjHello, InjRelease};
use std::os::unix::fs::PermissionsExt;

const XR24: u32 = fourcc(b'X', b'R', b'2', b'4');
const NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
const NVKMS: u32 = hostfd::NV_GEM_OBJECT_NVKMS;
/// An NVIDIA block-linear modifier, as GBM gives.
const MODIFIER: u64 = 0x0300_0000_0060_6015;

fn obj(size: u64, offset: u64) -> Obj {
    Obj {
        ty: NVKMS,
        size,
        gpu: 0,
        offset,
    }
}

fn rgb(w: u32, h: u32) -> InjImport {
    InjImport {
        nplanes: 1,
        width: w,
        height: h,
        fourcc: XR24,
        flags: 0,
        modifier: MODIFIER,
        offsets: [0; 4],
        strides: [w * 4, 0, 0, 0],
    }
}

fn setup() -> (Arc<FakeHost>, Registry) {
    let host = Arc::new(FakeHost::new(1));
    let reg = Registry::new(host.clone());
    (host, reg)
}

#[test]
fn a_well_formed_nvkms_buffer_is_accepted_and_opened_with_its_token() {
    let (host, reg) = setup();
    let d = host.dmabuf(obj(64 * 64 * 4, 0x10_0000));
    let (id, token) = reg.import(1, &rgb(64, 64), vec![d]).unwrap();
    assert_eq!((reg.live(), reg.bytes()), (1, 64 * 64 * 4));
    let o = reg.open(id, &token, 0).unwrap();
    // No IOCTL2 may adopt the number of a descriptor held here.
    assert!(crate::privfd::is_private(o.dmabuf.as_raw_fd()));
    assert_eq!((o.info.width, o.info.height, o.info.fourcc), (64, 64, XR24));
    assert_eq!(o.info.modifier, MODIFIER);
    assert_eq!(o.size, 64 * 64 * 4);
    assert!(reg.maps_live(0x10_0000, 4096));
    assert!(!reg.maps_live(0x10_0000 + 64 * 64 * 4, 4096));
}

#[test]
fn without_the_token_nothing_opens_and_a_missing_id_looks_the_same() {
    let (host, reg) = setup();
    let d = host.dmabuf(obj(4096 * 4, 0));
    let (id, token) = reg.import(1, &rgb(32, 32), vec![d]).unwrap();
    let mut wrong = token;
    wrong[15] ^= 1;
    assert_eq!(reg.open(id, &wrong, 0).unwrap_err(), libc::ENOENT);
    assert_eq!(reg.open(id, &[0; 16], 0).unwrap_err(), libc::ENOENT);
    assert_eq!(reg.open(id + 1, &token, 0).unwrap_err(), libc::ENOENT);
    assert_eq!(reg.open(0, &[0xff; 16], 0).unwrap_err(), libc::ENOENT);
    // A render file of another device is told so.
    assert_eq!(reg.open(id, &token, 1).unwrap_err(), libc::ENODEV);
    assert!(reg.open(id, &token, 0).is_ok());
}

#[test]
fn another_vms_backend_knows_nothing_of_this_ones_buffers() {
    let (host, reg) = setup();
    let d = host.dmabuf(obj(4096 * 4, 0));
    let (id, token) = reg.import(1, &rgb(32, 32), vec![d]).unwrap();
    let other = Registry::new(Arc::new(FakeHost::new(1)));
    assert_eq!(other.open(id, &token, 0).unwrap_err(), libc::ENOENT);
    assert_eq!(other.release(1, id).unwrap_err(), libc::ENOENT);
    assert_eq!(reg.live(), 1);
}

#[test]
fn what_is_not_a_dmabuf_or_not_this_gpus_nvkms_memory_is_refused() {
    let (host, reg) = setup();
    assert_eq!(
        reg.import(1, &rgb(32, 32), vec![host.not_dmabuf()]),
        Err(libc::EBADF)
    );
    // A foreign device's buffer (a udmabuf, an iGPU's): nvidia-drm
    // imports it as a dma-buf object.
    let foreign = host.dmabuf(Obj {
        ty: hostfd::NV_GEM_OBJECT_DMABUF,
        ..obj(4096 * 4, 0)
    });
    assert_eq!(
        reg.import(1, &rgb(32, 32), vec![foreign]),
        Err(libc::ENODEV)
    );
    // Another NVIDIA GPU's memory, when this GPU list has only one.
    let other_gpu = host.dmabuf(Obj {
        gpu: 1,
        ..obj(4096 * 4, 0)
    });
    assert_eq!(
        reg.import(1, &rgb(32, 32), vec![other_gpu]),
        Err(libc::ENODEV)
    );
    let user = host.dmabuf(Obj {
        ty: hostfd::NV_GEM_OBJECT_USERMEMORY,
        ..obj(4096 * 4, 0)
    });
    assert_eq!(reg.import(1, &rgb(32, 32), vec![user]), Err(libc::ENODEV));
    assert_eq!(reg.live(), 0);
    // What the refused imports made in the backend's file is closed.
    assert_eq!(host.closes(), 3);
}

#[test]
fn a_layout_the_object_cannot_hold_is_refused() {
    let (host, reg) = setup();
    let size = 64 * 64 * 4;
    let imp = |f: &dyn Fn(&mut InjImport)| {
        let mut i = rgb(64, 64);
        f(&mut i);
        let d = host.dmabuf(obj(size, 0));
        reg.import(1, &i, vec![d]).map(|_| ())
    };
    assert_eq!(imp(&|_| {}), Ok(()));
    assert_eq!(imp(&|i| i.height = 65), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.offsets[0] = 4), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.strides[0] = 64 * 4 - 1), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.strides[0] = u32::MAX), Err(libc::EINVAL));
    assert_eq!(
        imp(&|i| {
            i.offsets[0] = u32::MAX;
            i.strides[0] = u32::MAX;
        }),
        Err(libc::EINVAL)
    );
    assert_eq!(imp(&|i| i.width = 0), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.width = MAX_DIM + 1), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.fourcc = 0x1234_5678), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.flags = 2), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.modifier = MOD_INVALID), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.strides[1] = 4), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.nplanes = 2), Err(libc::EINVAL));
    assert_eq!(imp(&|i| i.nplanes = 5), Err(libc::EINVAL));
    assert_eq!(reg.live(), 1);
}

/// A helper whose buffer is slow to import holds up its own connection,
/// not a guest's INJECT_OPEN, which the VM's queue thread serves.
#[test]
fn a_slow_import_does_not_hold_up_an_open() {
    let (host, reg) = setup();
    let (id, token) = reg
        .import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))])
        .unwrap();
    let reg = Arc::new(reg);
    host.stall(true);
    let (h2, r2) = (host.clone(), reg.clone());
    let t = std::thread::spawn(move || {
        r2.import(2, &rgb(32, 32), vec![h2.dmabuf(obj(4096 * 4, 0x10000))])
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    let t0 = std::time::Instant::now();
    assert!(reg.open(id, &token, 0).is_ok());
    assert!(reg.maps_live(0, 1));
    assert!(t0.elapsed() < std::time::Duration::from_millis(40));
    host.stall(false);
    assert!(t.join().unwrap().is_ok());
    assert_eq!(reg.live(), 2);
}

#[test]
fn check_layout_wraps_nowhere() {
    let mut i = rgb(MAX_DIM, MAX_DIM);
    i.strides[0] = u32::MAX;
    i.offsets[0] = u32::MAX;
    assert_eq!(check_layout(&i, u64::MAX), Ok(()));
    assert_eq!(
        check_layout(&i, u64::from(u32::MAX) * 16384),
        Err(libc::EINVAL)
    );
}

#[test]
fn two_planes_must_be_one_object_and_the_chroma_plane_must_fit() {
    let (host, reg) = setup();
    let (w, h) = (64u32, 64u32);
    let size = u64::from(w * h * 3 / 2);
    let nv12 = InjImport {
        nplanes: 2,
        width: w,
        height: h,
        fourcc: NV12,
        flags: 0,
        modifier: 0,
        offsets: [0, w * h, 0, 0],
        strides: [w, w, 0, 0],
    };
    let d = host.dmabuf(obj(size, 0));
    let d2 = host.same_object(&d);
    assert!(reg.import(1, &nv12, vec![d, d2]).is_ok());
    // Two objects for two planes.
    let (a, b) = (host.dmabuf(obj(size, 0)), host.dmabuf(obj(size, 0x100000)));
    assert_eq!(reg.import(1, &nv12, vec![a, b]), Err(libc::EINVAL));
    // A descriptor per plane, no more, no fewer.
    let d = host.dmabuf(obj(size, 0));
    assert_eq!(reg.import(1, &nv12, vec![d]), Err(libc::EINVAL));
    // The chroma plane past the end.
    let mut past = nv12;
    past.offsets[1] = w * h + 1;
    let d = host.dmabuf(obj(size, 0));
    let d2 = host.same_object(&d);
    assert_eq!(reg.import(1, &past, vec![d, d2]), Err(libc::EINVAL));
}

#[test]
fn counts_and_bytes_are_bounded() {
    let host = Arc::new(FakeHost::new(1));
    let reg = Registry::with_limits(host.clone(), 3, 3 * 4096 * 4);
    for i in 0..3 {
        let d = host.dmabuf(obj(4096 * 4, i * 0x10000));
        reg.import(1, &rgb(32, 32), vec![d]).unwrap();
    }
    let d = host.dmabuf(obj(4096 * 4, 0x100000));
    assert_eq!(reg.import(1, &rgb(32, 32), vec![d]), Err(libc::ENOSPC));
    let reg = Registry::with_limits(host.clone(), 8, 2 * 4096 * 4);
    let d = host.dmabuf(obj(4096 * 4, 0));
    reg.import(1, &rgb(32, 32), vec![d]).unwrap();
    let big = host.dmabuf(obj(2 * 4096 * 4, 0x10000));
    assert_eq!(reg.import(1, &rgb(32, 32), vec![big]), Err(libc::EDQUOT));
    assert_eq!(reg.bytes(), 4096 * 4);
}

#[test]
fn release_is_the_importers_and_a_hangup_releases_everything_it_imported() {
    let (host, reg) = setup();
    let mut ids = Vec::new();
    for i in 0..3 {
        let d = host.dmabuf(obj(4096 * 4, i * 0x10000));
        ids.push(reg.import(7, &rgb(32, 32), vec![d]).unwrap());
    }
    let d = host.dmabuf(obj(4096 * 4, 0x100000));
    let (other, other_tok) = reg.import(8, &rgb(32, 32), vec![d]).unwrap();
    assert_eq!(reg.release(8, ids[0].0), Err(libc::ENOENT));
    assert_eq!(reg.release(7, ids[0].0), Ok(()));
    assert_eq!(reg.open(ids[0].0, &ids[0].1, 0).unwrap_err(), libc::ENOENT);
    assert_eq!(reg.release_peer(7), 2);
    assert_eq!(reg.live(), 1);
    assert!(reg.open(other, &other_tok, 0).is_ok());
    assert_eq!(reg.bytes(), 4096 * 4);
}

#[test]
fn one_dmabuf_injected_twice_is_one_handle_closed_with_the_last_id() {
    let (host, reg) = setup();
    let d = host.dmabuf(obj(4096 * 4, 0));
    let d2 = host.same_object(&d);
    let (a, _) = reg.import(1, &rgb(32, 32), vec![d]).unwrap();
    let (b, tb) = reg.import(1, &rgb(32, 32), vec![d2]).unwrap();
    reg.release(1, a).unwrap();
    assert_eq!(host.closes(), 0, "the second id still holds the handle");
    assert!(reg.open(b, &tb, 0).is_ok());
    reg.release(1, b).unwrap();
    assert_eq!(host.closes(), 1);
}

#[test]
fn opens_are_bounded_per_process_and_freed_by_close() {
    let mut bi = BackendInject::default();
    let p = |t| Owner::Proc {
        tgid: t,
        start_ns: 1,
    };
    for g in 0..MAX_OPENS / 4 {
        bi.record(1, g as u32 + 1, 0, 4096, p(1), None).unwrap();
    }
    assert_eq!(bi.record(1, 9999, 0, 4096, p(1), None), Err(libc::EAGAIN));
    // The same handle again is not a second open.
    assert_eq!(bi.record(1, 1, 0, 4096, p(1), None), Ok(()));
    assert!(bi.record(2, 1, 0, 4096, p(2), None).is_ok());
    bi.gem_closed(1, 1);
    assert!(bi.record(1, 9999, 0, 4096, p(1), None).is_ok());
    bi.file_closed(1);
    assert_eq!(bi.opens(), 1);
    assert!(bi.read_only(0, 1));
    bi.file_closed(2);
    assert!(!bi.read_only(0, 1));
}

// ── INJECT_OPEN, through the dispatcher ──

use crate::hostfd::HandleKind;
use crate::nvidia::NvidiaBackend;
use protocol::messages::*;

fn call(be: &mut NvidiaBackend, t: MsgType, handle: u32, body: &[u8]) -> Vec<u8> {
    let mut msg = crate::session::hdr(t, handle, 0, 0x77);
    msg.extend_from_slice(body);
    let mut resp = vec![0u8; 4096];
    let n = be.dispatch(&msg, &mut resp);
    resp.truncate(n);
    resp
}

fn status(resp: &[u8]) -> i32 {
    i32::from_le_bytes(resp[8..12].try_into().unwrap())
}

fn v2_backend(reg: Option<Arc<Registry>>) -> NvidiaBackend {
    let mut be = NvidiaBackend::for_test();
    be.set_inject(reg);
    let req = HelloReq {
        proto: PROTO_V2,
        flags: HELLO_F_FRESH,
        guest_caps: GCAP_PROC_ID,
        uvm_aperture_mib: 0,
    };
    let r = call(&mut be, MsgType::Hello, 0, crate::sys::pod::bytes(&req));
    assert_eq!(status(&r), 0);
    be
}

/// INJECT_OPEN as guest process `tgid`: status, result words, and the
/// description after them.
fn inject_open(
    be: &mut NvidiaBackend,
    render: u32,
    id: u32,
    token: &[u8; 16],
    tgid: u32,
) -> (i32, [u64; OP_MAX_RES], Vec<u8>) {
    let mut req = HostOpReq {
        op: OP_INJECT_OPEN,
        nargs: 4,
        args: [0; OP_MAX_ARGS],
    };
    req.args[0] = u64::from(render);
    req.args[1] = u64::from(id);
    req.args[2] = u64::from_le_bytes(token[..8].try_into().unwrap());
    req.args[3] = u64::from_le_bytes(token[8..].try_into().unwrap());
    let mut body = crate::sys::pod::bytes(&req).to_vec();
    body.extend_from_slice(crate::sys::pod::bytes(&ProcId {
        start_ns: 1,
        tgid,
        euid: 1000,
    }));
    let r = call(be, MsgType::HostOp, 0, &body);
    let h = size_of::<MsgHeader>();
    let resp: HostOpResp = crate::sys::pod::read(&r, h).unwrap_or_default();
    (
        status(&r),
        resp.res,
        r.get(h + 40..).unwrap_or(&[]).to_vec(),
    )
}

#[derive(Clone, Default)]
struct RecWindow(Arc<Mutex<Vec<(u64, bool)>>>);
impl crate::shm::WindowPlacer for RecWindow {
    fn place(
        &self,
        off: u64,
        _len: u64,
        _fd: std::os::fd::RawFd,
        _fo: u64,
        w: bool,
    ) -> crate::error::Result<()> {
        self.0.lock().unwrap().push((off, w));
        Ok(())
    }
    fn withdraw(&self, _off: u64, _len: u64) -> crate::error::Result<()> {
        Ok(())
    }
}

fn mmap(be: &mut NvidiaBackend, handle: u32, offset: u64, size: u64) -> (i32, MmapResp) {
    let req = MmapReq {
        size,
        offset,
        prot: 3,
        padding: 0,
    };
    let r = call(be, MsgType::Mmap, handle, crate::sys::pod::bytes(&req));
    let resp = crate::sys::pod::read(&r, size_of::<MsgHeader>()).unwrap_or_default();
    (status(&r), resp)
}

#[test]
fn a_guest_opens_an_injected_buffer_with_its_token_and_maps_it_read_only() {
    let host = Arc::new(FakeHost::new(1));
    let reg = Arc::new(Registry::new(host.clone()));
    let size = 32 * 32 * 4;
    let (id, token) = reg
        .import(1, &rgb(32, 32), vec![host.dmabuf(obj(size, 0x40_0000))])
        .unwrap();
    let mut be = v2_backend(Some(reg.clone()));
    let win = RecWindow::default();
    be.set_window(Box::new(win.clone()));
    let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));

    // Without the token, nothing: and nothing made in the file.
    let (st, _, _) = inject_open(&mut be, render, id, &[0; 16], 10);
    assert_eq!(st, -libc::ENOENT);
    assert_eq!(be.inject.opens(), 0);

    let (st, res, tail) = inject_open(&mut be, render, id, &token, 10);
    assert_eq!(st, 0);
    assert_ne!(res[0], 0, "a GEM handle");
    assert_eq!(res[1], size);
    assert_eq!(res[2], u64::from(NVKMS));
    assert_eq!(tail.len(), 64);
    assert_eq!(&tail[0..4], &32u32.to_le_bytes());
    assert_eq!(&tail[8..12], &XR24.to_le_bytes());
    assert_eq!(&tail[16..24], &MODIFIER.to_le_bytes());
    assert_eq!(&tail[40..44], &(32u32 * 4).to_le_bytes());
    assert_eq!(be.inject.opens(), 1);
    // The same open again is the same handle, one record.
    let (st, res2, _) = inject_open(&mut be, render, id, &token, 10);
    assert_eq!((st, res2[0]), (0, res[0]));
    assert_eq!(be.inject.opens(), 1);

    // Its mmap range is placed read-only, and the guest is told.
    let (st, m) = mmap(&mut be, render, 0x40_0000, size);
    assert_eq!(st, 0);
    assert_eq!(m.flags & MMAP_F_READ_ONLY, MMAP_F_READ_ONLY);
    assert_eq!(
        win.0.lock().unwrap().last(),
        Some(&(m.guest_phys_addr, false))
    );
    // Another object's is not.
    let (st, m) = mmap(&mut be, render, 0x80_0000, 4096);
    assert_eq!(st, 0);
    assert_eq!(m.flags & MMAP_F_READ_ONLY, 0);

    // After RELEASE, no new open; the guest's handle keeps the range
    // read-only while it is open.
    reg.release(1, id).unwrap();
    let other = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
    assert_eq!(inject_open(&mut be, other, id, &token, 10).0, -libc::ENOENT);
    assert!(be.inject.read_only(0x40_0000, 4096));
    call(&mut be, MsgType::Close, render, &[]);
    assert_eq!(be.inject.opens(), 0);
    assert!(!be.inject.read_only(0x40_0000, 4096));
}

#[test]
fn inject_open_is_refused_without_the_socket_on_other_files_and_other_devices() {
    let host = Arc::new(FakeHost::new(2));
    let reg = Arc::new(Registry::new(host.clone()));
    let (id, token) = reg
        .import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))])
        .unwrap();
    // A backend without --inject-socket.
    let mut be = v2_backend(None);
    let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
    assert_eq!(
        inject_open(&mut be, render, id, &token, 1).0,
        -libc::EOPNOTSUPP
    );
    // Another VM's backend, with an inject socket of its own.
    let mut other = v2_backend(Some(Arc::new(Registry::new(host.clone()))));
    let r = other.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
    assert_eq!(inject_open(&mut other, r, id, &token, 1).0, -libc::ENOENT);

    let mut be = v2_backend(Some(reg));
    let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
    let sync = be.adopt_for_test(host.not_dmabuf(), HandleKind::SyncFile);
    let second = be.adopt_for_test(host.render_file(1), HandleKind::DriRender(1));
    assert_eq!(inject_open(&mut be, sync, id, &token, 1).0, -libc::EBADF);
    assert_eq!(inject_open(&mut be, second, id, &token, 1).0, -libc::ENODEV);
    assert_eq!(inject_open(&mut be, render, id, &token, 1).0, 0);
}

#[test]
fn hello_offers_inject_only_with_the_socket() {
    let caps = |reg| {
        let mut be = NvidiaBackend::for_test();
        be.set_inject(reg);
        let req = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: 0,
            uvm_aperture_mib: 0,
        };
        let r = call(&mut be, MsgType::Hello, 0, crate::sys::pod::bytes(&req));
        crate::sys::pod::read::<HelloResp>(&r, size_of::<MsgHeader>())
            .unwrap()
            .backend_caps
    };
    assert_eq!(caps(None) & BCAP_INJECT, 0);
    let reg = Arc::new(Registry::new(Arc::new(FakeHost::new(1))));
    assert_eq!(caps(Some(reg)) & BCAP_INJECT, BCAP_INJECT);
}

#[test]
fn one_guest_process_cannot_take_every_open() {
    let host = Arc::new(FakeHost::new(1));
    let reg = Arc::new(Registry::new(host.clone()));
    let (id, token) = reg
        .import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))])
        .unwrap();
    let mut be = v2_backend(Some(reg));
    let mut n = 0;
    loop {
        let r = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
        let (st, _, _) = inject_open(&mut be, r, id, &token, 10);
        if st != 0 {
            assert_eq!(st, -libc::EAGAIN);
            break;
        }
        n += 1;
    }
    assert_eq!(n, MAX_OPENS / 4);
    let r = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
    assert_eq!(inject_open(&mut be, r, id, &token, 11).0, 0);
    // A refused open leaves no handle behind in the file.
    let r = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
    assert_eq!(inject_open(&mut be, r, id, &token, 10).0, -libc::EAGAIN);
    let (fd, _) = be.handles.get(r).unwrap();
    assert_eq!(host.handles_in(fd), 0);
    // And a handle the file had before the call is left as it was.
    let r = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
    let (fd, _) = be.handles.get(r).unwrap();
    let before = host.handles_in(fd);
    let opened = be.inject.registry().unwrap().open(id, &token, 0).unwrap();
    host.prime_import(fd, opened.dmabuf.as_fd()).unwrap();
    assert_eq!(host.handles_in(fd), before + 1);
    let closes = host.closes();
    assert_eq!(inject_open(&mut be, r, id, &token, 10).0, -libc::EAGAIN);
    assert_eq!(host.closes(), closes, "nothing closed in the caller's file");
}

/// nvidia-drm imports another NVIDIA device's buffer as a duplicate,
/// an NVKMS object of its own device: IDENTIFY cannot tell. Only the
/// device whose import is the helper's very object takes it.
#[test]
fn another_nvidia_devices_buffer_is_its_own_not_a_duplicates() {
    let host = Arc::new(FakeHost::new(2));
    let reg = Registry::new(host.clone());
    let (id, token) = reg
        .import(
            1,
            &rgb(32, 32),
            vec![host.dmabuf(Obj {
                gpu: 1,
                ..obj(4096 * 4, 0)
            })],
        )
        .unwrap();
    // Node 0's duplicate was made, refused and closed.
    assert_eq!(host.closes(), 1);
    assert_eq!(reg.open(id, &token, 0).unwrap_err(), libc::ENODEV);
    assert!(reg.open(id, &token, 1).is_ok());
    // With one GPU in the list, another GPU's buffer is nobody's.
    let h1 = Arc::new(FakeHost::new(1));
    let one = Registry::new(h1.clone());
    assert_eq!(
        one.import(
            1,
            &rgb(32, 32),
            vec![h1.dmabuf(Obj {
                gpu: 1,
                ..obj(4096 * 4, 0)
            })]
        ),
        Err(libc::ENODEV)
    );
}

/// No export hands out an injected buffer: HOST_OP PRIME_EXPORT of the
/// handle INJECT_OPEN made is refused before the host is asked, and the
/// dma-buf any other path would export is recognised (the taint set).
#[test]
fn an_injected_buffer_is_never_exported() {
    let host = Arc::new(FakeHost::new(1));
    let reg = Arc::new(Registry::new(host.clone()));
    let d = host.dmabuf(obj(4096 * 4, 0));
    let helper_copy = d.try_clone().unwrap();
    let (id, token) = reg.import(1, &rgb(32, 32), vec![d]).unwrap();
    let mut be = v2_backend(Some(reg.clone()));
    let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
    let (st, res, _) = inject_open(&mut be, render, id, &token, 10);
    assert_eq!(st, 0);
    let prime_export = |be: &mut NvidiaBackend, gem: u64| {
        let mut req = HostOpReq {
            op: OP_PRIME_EXPORT,
            nargs: 2,
            args: [0; OP_MAX_ARGS],
        };
        req.args[0] = u64::from(render);
        req.args[1] = gem;
        status(&call(be, MsgType::HostOp, 0, crate::sys::pod::bytes(&req)))
    };
    assert_eq!(prime_export(&mut be, res[0]), -libc::EINVAL);
    // Another handle reaches the host (a memfd: no ioctls).
    assert_eq!(prime_export(&mut be, res[0] + 100), -libc::ENOTTY);
    // The helper's dma-buf is tainted while the id or an open holds it.
    let taint = be.inject_taint.clone();
    assert!(!exportable(&taint, helper_copy.as_fd()));
    assert!(exportable(&taint, host.not_dmabuf().as_fd()));
    reg.release(1, id).unwrap();
    assert!(
        !exportable(&taint, helper_copy.as_fd()),
        "the open still holds it"
    );
    call(&mut be, MsgType::Close, render, &[]);
    assert!(exportable(&taint, helper_copy.as_fd()));
    assert!(reg.taint().is_empty());
}

/// An injected object is never held untainted: an IMPORT, or an
/// INJECT_OPEN, whose hold on the taint set cannot be had (no descriptor
/// left to keep the dma-buf by) is refused, and nothing of it is kept --
/// no id, and no handle in the guest's file. Before, both went ahead
/// untainted, and the open's handle could be re-homed into a KMS file once
/// the helper released the id.
#[test]
fn nothing_injected_is_kept_without_its_taint() {
    let host = Arc::new(FakeHost::new(1));
    let reg = Arc::new(Registry::new(host.clone()));
    let set = |on| {
        reg.taint()
            .refuse_holds
            .store(on, std::sync::atomic::Ordering::Relaxed)
    };
    set(true);
    let closes = host.closes();
    let r = reg.import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))]);
    assert_eq!(r, Err(libc::EMFILE));
    assert_eq!((reg.live(), reg.bytes()), (0, 0));
    assert_eq!(host.closes(), closes + 1, "the backend's import let go");

    set(false);
    let (id, token) = reg
        .import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))])
        .unwrap();
    let mut be = v2_backend(Some(reg.clone()));
    let file = host.render_file(0);
    let view = file.try_clone().unwrap();
    let render = be.adopt_for_test(file, HandleKind::DriRender(0));
    set(true);
    assert_eq!(
        inject_open(&mut be, render, id, &token, 10).0,
        -libc::EMFILE
    );
    assert_eq!(host.handles_in(view.as_fd()), 0, "nothing imported");
    assert_eq!(be.inject.opens(), 0);
    set(false);

    // Opened twice, one record and one hold: once the id and the file go,
    // nothing is left tainted.
    for _ in 0..2 {
        assert_eq!(inject_open(&mut be, render, id, &token, 10).0, 0);
    }
    assert_eq!(be.inject.opens(), 1);
    reg.release(1, id).unwrap();
    assert!(!reg.taint().is_empty(), "the open holds it");
    call(&mut be, MsgType::Close, render, &[]);
    assert!(
        reg.taint().is_empty(),
        "a second open's hold was given back"
    );
}

#[test]
fn a_syncobj_is_injected_opened_with_its_token_and_released() {
    let (host, reg) = setup();
    // Only a syncobj file, and exactly one.
    assert_eq!(
        reg.import_syncobj(1, 0, vec![host.not_dmabuf()]),
        Err(libc::EBADF)
    );
    assert_eq!(
        reg.import_syncobj(1, 0, Vec::<OwnedFd>::new()),
        Err(libc::EINVAL)
    );
    assert_eq!(
        reg.import_syncobj(1, 0, vec![host.syncobj(), host.syncobj()]),
        Err(libc::EINVAL)
    );
    assert_eq!(
        reg.import_syncobj(1, 1, vec![host.syncobj()]),
        Err(libc::EINVAL)
    );
    let (id, token) = reg.import_syncobj(1, 0, vec![host.syncobj()]).unwrap();
    assert!(reg.open_syncobj(id, &token).is_ok());
    let mut wrong = token;
    wrong[0] ^= 1;
    assert_eq!(reg.open_syncobj(id, &wrong).unwrap_err(), libc::ENOENT);
    // A syncobj id is no buffer's, and a buffer's no syncobj's.
    assert_eq!(reg.open(id, &token, 0).unwrap_err(), libc::ENOENT);
    let (bid, btok) = reg
        .import(1, &rgb(32, 32), vec![host.dmabuf(obj(4096 * 4, 0))])
        .unwrap();
    assert_ne!(bid, id);
    assert_eq!(reg.open_syncobj(bid, &btok).unwrap_err(), libc::ENOENT);
    // Only the importer releases it; its hangup does too.
    assert_eq!(reg.release(2, id), Err(libc::ENOENT));
    assert_eq!(reg.release(1, id), Ok(()));
    assert_eq!(reg.open_syncobj(id, &token).unwrap_err(), libc::ENOENT);
    reg.import_syncobj(1, 0, vec![host.syncobj()]).unwrap();
    assert_eq!(reg.release_peer(1), 2);
    assert_eq!(reg.syncobjs(), 0);
    // Bounded.
    for _ in 0..MAX_SYNCOBJS {
        reg.import_syncobj(3, 0, vec![host.syncobj()]).unwrap();
    }
    assert_eq!(
        reg.import_syncobj(3, 0, vec![host.syncobj()]),
        Err(libc::ENOSPC)
    );
}

#[test]
fn a_guest_opens_an_injected_syncobj_into_its_render_file() {
    let host = Arc::new(FakeHost::new(1));
    let reg = Arc::new(Registry::new(host.clone()));
    let (id, token) = reg.import_syncobj(1, 0, vec![host.syncobj()]).unwrap();
    let mut be = v2_backend(Some(reg.clone()));
    let render = be.adopt_for_test(host.render_file(0), HandleKind::DriRender(0));
    let open = |be: &mut NvidiaBackend, render: u32, token: &[u8; 16]| {
        let mut req = HostOpReq {
            op: OP_INJECT_OPEN_SYNCOBJ,
            nargs: 4,
            args: [0; OP_MAX_ARGS],
        };
        req.args[0] = u64::from(render);
        req.args[1] = u64::from(id);
        req.args[2] = u64::from_le_bytes(token[..8].try_into().unwrap());
        req.args[3] = u64::from_le_bytes(token[8..].try_into().unwrap());
        let r = call(be, MsgType::HostOp, 0, crate::sys::pod::bytes(&req));
        let resp: HostOpResp =
            crate::sys::pod::read(&r, size_of::<MsgHeader>()).unwrap_or_default();
        (status(&r), resp.res[0])
    };
    assert_eq!(open(&mut be, render, &[0; 16]).0, -libc::ENOENT);
    let (st, h) = open(&mut be, render, &token);
    assert_eq!(st, 0);
    assert_ne!(h, 0);
    let (fd, _) = be.handles.get(render).unwrap();
    assert_eq!(host.syncobjs_in(fd), 1);
    // The file is an importer now: its syncobjs' registrations are not
    // dropped by a DESTROY (fence.rs).
    assert!(!be.syncobj_regs.is_private_for_test(render, h as u32));
    // Not on a file that is not a render file.
    let sync = be.adopt_for_test(host.not_dmabuf(), HandleKind::SyncFile);
    assert_eq!(open(&mut be, sync, &token).0, -libc::EBADF);
}

// ── the socket ──

/// A directory of this test's own (the tests run in parallel).
fn tmpdir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "nvinject-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn server(uid: u32) -> (PathBuf, Arc<FakeHost>, InjectServer) {
    let dir = tmpdir();
    let path = dir.join("inject.sock");
    let host = Arc::new(FakeHost::new(1));
    let reg = Arc::new(Registry::new(host.clone()));
    let s = InjectServer::bind_idle(&path, uid, reg).unwrap();
    s.start().unwrap();
    (path, host, s)
}

fn roundtrip(c: &OwnedFd, req: &[u8], fds: &[std::os::fd::RawFd]) -> Option<InjReply> {
    crate::sys::net::send_packet(c.as_raw_fd(), req, fds).ok()?;
    let mut b = [0u8; 64];
    let p = crate::sys::net::recv_packet(c.as_raw_fd(), &mut b).ok()?;
    (p.len == INJ_REPLY_SIZE).then(|| InjReply::from_bytes(&b[..p.len]).unwrap())
}

fn hello_bytes() -> [u8; 16] {
    InjHello {
        version: INJ_VERSION,
        flags: 0,
    }
    .to_bytes()
}

#[test]
fn the_socket_is_private_and_speaks_hello_import_release() {
    let (path, host, s) = server(crate::sys::proc::uid());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let c = crate::sys::net::seqpacket_connect(&path).unwrap();
    // Nothing before HELLO.
    let d = host.dmabuf(obj(4096 * 4, 0));
    assert!(roundtrip(&c, &rgb(32, 32).to_bytes(), &[d.as_raw_fd()]).is_none());
    let c = crate::sys::net::seqpacket_connect(&path).unwrap();
    let h = roundtrip(&c, &hello_bytes(), &[]).unwrap();
    assert_eq!((h.status, h.version), (0, INJ_VERSION));
    assert_eq!(h.max_buffers as usize, MAX_BUFFERS);
    assert_eq!(h.max_syncobjs as usize, MAX_SYNCOBJS);
    assert_eq!(h.max_bytes, MAX_BYTES);
    let r = roundtrip(&c, &rgb(32, 32).to_bytes(), &[d.as_raw_fd()]).unwrap();
    assert_eq!((r.op, r.status), (INJ_OP_IMPORT, 0));
    assert_ne!(r.id, 0);
    assert!(s.registry().open(r.id, &r.token, 0).is_ok());
    // A refusal keeps the connection.
    let bad = roundtrip(
        &c,
        &rgb(32, 32).to_bytes(),
        &[host.not_dmabuf().as_raw_fd()],
    );
    assert_eq!(bad.unwrap().status, -libc::EBADF);
    let rel = roundtrip(&c, &InjRelease { id: r.id }.to_bytes(), &[]).unwrap();
    assert_eq!(rel.status, 0);
    assert_eq!(s.registry().live(), 0);
    let rel = roundtrip(&c, &InjRelease { id: r.id }.to_bytes(), &[]).unwrap();
    assert_eq!(rel.status, -libc::ENOENT);
    s.shutdown();
}

#[test]
fn a_hangup_releases_what_the_helper_imported() {
    let (path, host, s) = server(crate::sys::proc::uid());
    let c = crate::sys::net::seqpacket_connect(&path).unwrap();
    roundtrip(&c, &hello_bytes(), &[]).unwrap();
    for i in 0..3 {
        let d = host.dmabuf(obj(4096 * 4, i * 0x10000));
        assert_eq!(
            roundtrip(&c, &rgb(32, 32).to_bytes(), &[d.as_raw_fd()])
                .unwrap()
                .status,
            0
        );
    }
    assert_eq!(s.registry().live(), 3);
    drop(c);
    for _ in 0..200 {
        if s.registry().live() == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(s.registry().live(), 0);
    s.shutdown();
}

#[test]
fn a_peer_of_another_uid_is_hung_up_on() {
    let (path, _host, s) = server(crate::sys::proc::uid().wrapping_add(1));
    let c = crate::sys::net::seqpacket_connect(&path).unwrap();
    assert!(roundtrip(&c, &hello_bytes(), &[]).is_none());
    s.shutdown();
}

/// A helper of the VMM's uid is refused even when `--inject-uid` names
/// it: one helper uid per VM, never the VMM's.
#[test]
fn a_peer_of_the_vmms_uid_is_hung_up_on() {
    let (path, _host, s) = server(crate::sys::proc::uid());
    s.refuse_uid(crate::sys::proc::uid());
    let c = crate::sys::net::seqpacket_connect(&path).unwrap();
    assert!(roundtrip(&c, &hello_bytes(), &[]).is_none());
    s.shutdown();
}

#[test]
fn malformed_packets_end_the_connection() {
    let (path, host, s) = server(crate::sys::proc::uid());
    let send = |bytes: &[u8], fds: &[std::os::fd::RawFd]| {
        let c = crate::sys::net::seqpacket_connect(&path).unwrap();
        roundtrip(&c, &hello_bytes(), &[]).unwrap();
        roundtrip(&c, bytes, fds)
    };
    let d = host.dmabuf(obj(4096 * 4, 0));
    // A short IMPORT, an unknown op, descriptors on RELEASE, a second
    // HELLO, an oversized packet.
    assert!(send(&rgb(32, 32).to_bytes()[..60], &[d.as_raw_fd()]).is_none());
    assert!(send(&[9, 0, 0, 0, 0, 0, 0, 0], &[]).is_none());
    assert!(send(&InjRelease { id: 1 }.to_bytes(), &[d.as_raw_fd()]).is_none());
    assert_eq!(send(&hello_bytes(), &[]).unwrap().status, -libc::EPROTO);
    assert!(send(&[2u8; 80], &[]).is_none());
    // More descriptors than planes: refused, the connection kept.
    let d2 = host.dmabuf(obj(4096 * 4, 0x10000));
    assert_eq!(
        send(&rgb(32, 32).to_bytes(), &[d.as_raw_fd(), d2.as_raw_fd()])
            .unwrap()
            .status,
        -libc::EINVAL
    );
    // A wrong version.
    let c = crate::sys::net::seqpacket_connect(&path).unwrap();
    let v2 = InjHello {
        version: 2,
        flags: 0,
    };
    assert_eq!(
        roundtrip(&c, &v2.to_bytes(), &[]).unwrap().status,
        -libc::EPROTO
    );
    s.shutdown();
}

#[test]
fn no_more_than_max_peers_at_once() {
    let (path, _host, s) = server(crate::sys::proc::uid());
    let conns: Vec<_> = (0..MAX_PEERS)
        .map(|_| {
            let c = crate::sys::net::seqpacket_connect(&path).unwrap();
            roundtrip(&c, &hello_bytes(), &[]).unwrap();
            c
        })
        .collect();
    let extra = crate::sys::net::seqpacket_connect(&path).unwrap();
    assert!(roundtrip(&extra, &hello_bytes(), &[]).is_none());
    drop(conns);
    s.shutdown();
}

/// The real host tells a helper's dma-buf the way the rest of the backend
/// tells one (`hostfd::classify`), asking no filesystem: memory, pipes,
/// eventfds and files are refused, and a udmabuf (where /dev/udmabuf is
/// open to us) is taken. `fstatfs`, which a FUSE server answers on its own
/// schedule, is not asked.
#[test]
#[cfg_attr(miri, ignore = "Miri has no memfd or /dev/udmabuf")]
fn the_real_hosts_dmabuf_check_asks_no_filesystem() {
    let h = SysInjectHost::for_this_host();
    let memfd =
        crate::sys::fd::memfd(c"inject", libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING).unwrap();
    crate::sys::fd::ftruncate(&memfd, 4096).unwrap();
    let (r, _w) = std::io::pipe().unwrap();
    let file: OwnedFd = std::fs::File::open("/proc/self/status").unwrap().into();
    for fd in [memfd.as_fd(), r.as_fd(), file.as_fd()] {
        assert!(!h.is_dmabuf(fd));
    }
    let Ok(dev) = crate::sys::fd::open_path("/dev/udmabuf", libc::O_RDWR) else {
        eprintln!("SKIPPED the udmabuf half: no /dev/udmabuf");
        return;
    };
    crate::sys::fd::add_seals(&memfd, libc::F_SEAL_SHRINK).unwrap();
    let d = crate::sys::ioctl::udmabuf_create(dev.as_fd(), memfd.as_fd(), 4096, 1).unwrap();
    assert!(h.is_dmabuf(d.as_fd()));
}
