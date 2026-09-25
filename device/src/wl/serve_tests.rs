//! The Wayland channel through the dispatcher: OPEN(DEV_WAYLAND), WL_SEND,
//! WL_RECV and CLOSE as the guest kernel sends them, served by
//! `NvidiaBackend::serve` against a fake compositor on a real socket.

use std::collections::VecDeque;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::messages::*;
use wlwire::engine::{Engine, EngineConfig, Local, Side};
use wlwire::frame::{self, Desc};
use wlwire::policy::{LeaseGate, Policy};
use wlwire::proto::op;
use wlwire::sys;
use wlwire::wire::MsgBuilder;

use super::conn::{RecvOps, SendOps, WlConfig};
use super::export::WlExport;
use super::serve::{TableRecv, TableSend};
use super::tests::{
    FakeHost, GuestPlat, fake_compositor, get_registry, global_names, msgs, sync_done, tmpdir,
};
use crate::handle_table::HandleTable;
use crate::hostfd::HandleKind;
use crate::nvidia::NvidiaBackend;
use crate::pump::{PumpCmd, WatchMode};
use crate::session::hdr;

const HDR: usize = size_of::<MsgHeader>();

fn bytes_of<T: Copy>(v: &T) -> &[u8] {
    // SAFETY: a plain-old-data wire struct viewed as its bytes.
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

fn msg(t: MsgType, handle: u32, body: &[u8]) -> Vec<u8> {
    let mut v = hdr(t, handle, 0, 0x77);
    v.extend_from_slice(body);
    v
}

/// Serve one message with `cap` bytes posted for the answer.
fn call(be: &mut NvidiaBackend, t: MsgType, handle: u32, body: &[u8], cap: usize) -> Vec<u8> {
    let mut resp = vec![0u8; cap];
    let n = be.dispatch(&msg(t, handle, body), &mut resp);
    resp.truncate(n);
    resp
}

fn status(r: &[u8]) -> i32 {
    i32::from_le_bytes(r[8..12].try_into().unwrap())
}

fn resp_handle(r: &[u8]) -> u32 {
    u32::from_le_bytes(r[4..8].try_into().unwrap())
}

fn backend() -> NvidiaBackend {
    let mut be = NvidiaBackend::for_test();
    be.set_host_nodes_for_test(Vec::new(), Vec::new());
    be.set_wl_host_for_test(Arc::new(FakeHost));
    be
}

fn hello(be: &mut NvidiaBackend) {
    let req = HelloReq {
        proto: PROTO_V2,
        flags: HELLO_F_FRESH,
        ..Default::default()
    };
    assert_eq!(
        status(&call(be, MsgType::Hello, 0, bytes_of(&req), 4096)),
        0
    );
}

/// OPEN(DEV_WAYLAND, mode): the status, and the handle it returned.
fn open(be: &mut NvidiaBackend, mode: u32) -> (i32, u32) {
    let req = OpenReq {
        device_type: DEV_WAYLAND,
        flags: mode,
    };
    let r = call(be, MsgType::Open, 0, bytes_of(&req), 64);
    (status(&r), resp_handle(&r))
}

fn close(be: &mut NvidiaBackend, h: u32) -> i32 {
    status(&call(be, MsgType::Close, h, &[], 64))
}

/// Room for one whole frame of `max` bytes, as nvgpu_wl.c posts it.
fn recv(be: &mut NvidiaBackend, h: u32, max: u32, cap: usize) -> Vec<u8> {
    let req = WlRecvReq {
        max_bytes: max,
        max_desc: frame::MAX_DESC as u32,
    };
    call(be, MsgType::WlRecv, h, bytes_of(&req), cap)
}

const MAX: u32 = 256 << 10;

/// The guest daemon's engine, speaking to the backend through serve().
struct Guest {
    e: Engine,
    h: u32,
}

impl Guest {
    fn new(be: &mut NvidiaBackend, h: u32, drm_file: bool) -> Self {
        let caps = if drm_file { frame::HELLO_G_DRM_FILE } else { 0 };
        Self::with_caps(be, h, caps)
    }

    /// A guest whose HELLO says `caps` (and whose own engine allows what
    /// they say).
    fn with_caps(be: &mut NvidiaBackend, h: u32, caps: u32) -> Self {
        let mut e = Engine::new(EngineConfig {
            side: Side::Guest,
            local: Local::Client,
            policy: Policy {
                drm_file: caps & frame::HELLO_G_DRM_FILE != 0,
                lease: LeaseGate::Allow,
                fences: caps & frame::HELLO_G_SYNCOBJ != 0,
            },
            rewrites: None,
            synth_released: true,
        });
        e.hello(caps);
        let mut g = Guest { e, h };
        g.flush(be);
        g
    }

    fn flush(&mut self, be: &mut NvidiaBackend) {
        let mut q = self.e.take_units();
        while !q.is_empty() {
            let (f, _) = frame::pack(&mut q, MAX as usize, frame::MAX_DESC, false);
            let r = call(be, MsgType::WlSend, self.h, &f, 64);
            assert_eq!(status(&r), 0, "WL_SEND");
            let resp: WlSendResp =
                unsafe { (r[HDR..].as_ptr() as *const WlSendResp).read_unaligned() };
            assert_eq!(resp.accepted as usize, f.len());
        }
    }

    fn client(&mut self, be: &mut NvidiaBackend, m: &[Vec<u8>]) {
        let mut data = m.concat();
        self.e
            .from_local(&mut data, &mut VecDeque::new(), &mut GuestPlat)
            .unwrap();
        self.flush(be);
    }

    /// WL_RECV until `pred` holds for what reached the client. Returns that,
    /// and every descriptor the frames carried (as the backend described it).
    fn recv_until(
        &mut self,
        be: &mut NvidiaBackend,
        pred: impl Fn(&[u8], &[Desc]) -> bool,
    ) -> (Vec<u8>, Vec<Desc>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = Vec::new();
        let mut descs = Vec::new();
        loop {
            let r = recv(be, self.h, MAX, HDR + MAX as usize);
            assert_eq!(status(&r), 0, "WL_RECV");
            let f = &r[HDR..];
            let d = frame::decode(f).unwrap();
            let n = d.descs.len();
            descs.extend(d.descs.iter().copied());
            if !d.records.is_empty() {
                self.e
                    .from_channel(f, (0..n).map(|_| None).collect(), &mut GuestPlat)
                    .unwrap();
                for (b, _) in self.e.local_out().drain() {
                    bytes.extend(b);
                }
            }
            if pred(&bytes, &descs) {
                return (bytes, descs);
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn watched_legacy(cmds: &[PumpCmd], h: u32) -> bool {
    cmds.iter().any(
        |c| matches!(c, PumpCmd::Watch { handle, mode: WatchMode::Legacy, .. } if *handle == h),
    )
}

#[test]
fn a_guest_client_reaches_the_compositor_through_open_send_and_recv() {
    let dir = tmpdir("serve-reg");
    let sock = dir.join("wl");
    fake_compositor(
        sock.clone(),
        vec![
            (1, "wl_compositor", 6),
            (2, "zwlr_screencopy_manager_v1", 3),
        ],
        vec![],
    );
    let mut be = backend();
    be.set_wayland(Some(WlConfig::new(&sock)));
    hello(&mut be);
    be.take_pump_cmds();

    let (st, h) = open(&mut be, frame::WL_OPEN_CONNECT);
    assert_eq!(st, 0);
    assert_eq!(be.handles.kind(h), Some(HandleKind::Wayland));
    // Watched from the OPEN on, the legacy way: nvgpu_wl.c never WATCHes.
    assert!(watched_legacy(&be.take_pump_cmds(), h));
    // What the table holds is the readiness eventfd, never the socket.
    let (fd, _) = be.handles.get(h).unwrap();
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
    assert_eq!(link.to_string_lossy(), "anon_inode:[eventfd]");

    let mut g = Guest::new(&mut be, h, false);
    g.client(&mut be, &get_registry());
    let (b, _) = g.recv_until(&mut be, |b, _| sync_done(b, 0));
    assert_eq!(global_names(&b), vec!["wl_compositor"]);
}

#[test]
fn wl_recv_refuses_a_buffer_it_could_not_fill_before_taking_anything() {
    let dir = tmpdir("serve-small");
    let sock = dir.join("wl");
    let _l = UnixListener::bind(&sock).unwrap();
    let mut be = backend();
    be.set_wayland(Some(WlConfig::new(&sock)));
    hello(&mut be);
    let (_, h) = open(&mut be, frame::WL_OPEN_CONNECT);

    // Asks for more than it posted room for.
    let r = recv(&mut be, h, MAX, HDR + MAX as usize - 1);
    assert_eq!(status(&r), -libc::EMSGSIZE);
    // Smaller than the largest record the engine makes.
    let small = frame::MIN_FRAME as u32 - 1;
    let r = recv(&mut be, h, small, HDR + small as usize);
    assert_eq!(status(&r), -libc::EINVAL);
    // Neither took anything: the backend's HELLO record is still first.
    let r = recv(&mut be, h, MAX, HDR + MAX as usize);
    assert_eq!(status(&r), 0);
    let f = frame::decode(&r[HDR..]).unwrap();
    assert_eq!(f.records().next().map(|r| r.ty), Some(frame::REC_HELLO));
}

#[test]
fn a_lease_of_our_gpu_arrives_as_a_nonblocking_handle_in_the_table() {
    let dir = tmpdir("serve-lease");
    let sock = dir.join("wl");
    fake_compositor(
        sock.clone(),
        vec![
            (40, "wp_drm_lease_device_v1", 1),
            (41, "wp_drm_lease_device_v1", 1),
        ],
        vec![(40, "lease-ours"), (41, "lease-other-gpu")],
    );
    let mut be = backend();
    let mut cfg = WlConfig::new(&sock);
    cfg.allow_lease = true;
    be.set_wayland(Some(cfg));
    hello(&mut be);
    let (_, h) = open(&mut be, frame::WL_OPEN_CONNECT);
    let mut g = Guest::new(&mut be, h, true);
    g.client(&mut be, &get_registry());
    let (b, _) = g.recv_until(&mut be, |b, _| sync_done(b, 0));
    assert_eq!(global_names(&b), vec!["wp_drm_lease_device_v1"]);

    g.client(
        &mut be,
        &[MsgBuilder::new(2, op::wl_registry::REQ_BIND)
            .uint(40)
            .generic_new_id("wp_drm_lease_device_v1", 1, 5)
            .finish()],
    );
    let (_, descs) = g.recv_until(&mut be, |_, d| !d.is_empty());
    let d = descs[0];
    assert_eq!(d.kind, frame::DESC_DRM_FILE);
    assert!(!d.is_invalid());
    assert_eq!(d.b, HK_DRM_LEASE);
    assert_eq!(be.handles.kind(d.a), Some(HandleKind::DrmLease(0)));
    let (fd, _) = be.handles.get(d.a).unwrap();
    let fl = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(fl & libc::O_NONBLOCK, 0, "the pump must never block on it");
    // The guest owns it now, like any other handle.
    assert_eq!(close(&mut be, d.a), 0);
}

#[test]
fn a_dmabuf_is_exported_only_on_a_render_handle_of_the_session() {
    let mut t = HandleTable::new();
    let ev = t
        .insert(crate::hostfd::new_eventfd().unwrap(), HandleKind::Eventfd)
        .unwrap();
    // SAFETY: a NUL-terminated path; ownership passes to the OwnedFd.
    let null = unsafe {
        OwnedFd::from_raw_fd(libc::open(
            c"/dev/null".as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC,
        ))
    };
    let render = t.insert(null, HandleKind::DriRender(0)).unwrap();
    let mut ops = TableSend { handles: &t };
    let e = |r: std::io::Result<OwnedFd>| r.unwrap_err().raw_os_error();
    assert_eq!(e(ops.prime_export(ev, 1)), Some(libc::EBADF));
    assert_eq!(e(ops.prime_export(12345, 1)), Some(libc::EBADF));
    // A render handle reaches the host: /dev/null answers the ioctl itself.
    assert_eq!(e(ops.prime_export(render, 1)), Some(libc::ENOTTY));
}

/// Explicit sync is offered when this backend serves fences (BCAP_FENCES,
/// `BackendConfig::fences`) and the guest's HELLO says its kernel can name a
/// client's syncobj by its host syncobj -- both, and only both.
#[test]
fn the_syncobj_global_is_offered_with_fences_served_and_a_guest_that_can_name_syncobjs() {
    let dir = tmpdir("serve-syncobj");
    let sock = dir.join("wl");
    fake_compositor(
        sock.clone(),
        vec![
            (1, "wl_compositor", 6),
            (2, "wp_linux_drm_syncobj_manager_v1", 1),
        ],
        vec![],
    );
    for (fences, caps, offered) in [
        (true, frame::HELLO_G_SYNCOBJ, true),
        (true, 0, false),
        (false, frame::HELLO_G_SYNCOBJ, false),
    ] {
        let mut be = backend();
        be.config.fences = fences;
        be.set_wayland(Some(WlConfig::new(&sock)));
        hello(&mut be);
        let (st, h) = open(&mut be, frame::WL_OPEN_CONNECT);
        assert_eq!(st, 0);
        let mut g = Guest::with_caps(&mut be, h, caps);
        g.client(&mut be, &get_registry());
        let (b, _) = g.recv_until(&mut be, |b, _| sync_done(b, 0));
        let names = global_names(&b);
        assert_eq!(
            names.iter().any(|n| n == "wp_linux_drm_syncobj_manager_v1"),
            offered,
            "fences {fences}, caps {caps:#x}: {names:?}"
        );
        assert_eq!(close(&mut be, h), 0);
    }
}

#[test]
fn a_syncobj_for_the_compositor_must_be_a_syncobj_handle_of_the_session() {
    let mut t = HandleTable::new();
    let ev = t
        .insert(crate::hostfd::new_eventfd().unwrap(), HandleKind::Eventfd)
        .unwrap();
    // Classified by the table's kind, never by the file: stand-in syncobj.
    let so = t
        .insert(sys::memfd(c"syncobj", 0).unwrap(), HandleKind::Syncobj)
        .unwrap();
    let mut ops = TableSend { handles: &t };
    let fd = ops.syncobj(so).unwrap();
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap();
    assert!(link.to_string_lossy().contains("syncobj"), "{link:?}");
    for h in [ev, 12345] {
        assert_eq!(
            ops.syncobj(h).unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
    }
}

#[test]
fn a_compositor_descriptor_is_adopted_only_if_it_is_what_its_desc_says() {
    let mut t = HandleTable::new();
    let host = FakeHost;
    let mut ops = TableRecv {
        handles: &mut t,
        host: &host,
        created: Vec::new(),
    };
    let fd = |name: &std::ffi::CStr| sys::memfd(name, 0).unwrap();
    let (h, hk) = ops.adopt(fd(c"lease-ours"), frame::DESC_DRM_FILE).unwrap();
    assert_eq!(hk, HK_DRM_LEASE);
    let (d, dk) = ops.adopt(fd(c"a-dmabuf"), frame::DESC_DMABUF).unwrap();
    assert_eq!(dk, HK_DMABUF);
    // A DRM file of another GPU, a dma-buf posing as a lease, a lease posing
    // as a dma-buf: none reaches the table.
    for (name, kind) in [
        (c"lease-other-gpu", frame::DESC_DRM_FILE),
        (c"a-dmabuf", frame::DESC_DRM_FILE),
        (c"lease-ours", frame::DESC_DMABUF),
        (c"lease-ours", frame::DESC_SHM_POOL),
    ] {
        assert_eq!(
            ops.adopt(fd(name), kind).unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
    }
    assert_eq!(ops.created, vec![h, d]);
    assert_eq!(t.len(), 2);
}

#[test]
fn a_dmabuf_naming_a_foreign_owner_reaches_the_compositor_as_a_placeholder() {
    let dir = tmpdir("serve-foreign");
    let sock = dir.join("wl");
    let l = UnixListener::bind(&sock).unwrap();
    let mut be = backend();
    be.set_wayland(Some(WlConfig::new(&sock)));
    hello(&mut be);
    let (_, h) = open(&mut be, frame::WL_OPEN_CONNECT);
    let (server, _) = l.accept().unwrap();
    // The guest's GuestPlat names owner 7, gem 42; make 7 anything but a
    // render file: the next handle the table gives out is not 7, so put
    // eventfds in until it is.
    while be.handles.kind(7).is_none() {
        be.adopt_for_test(crate::hostfd::new_eventfd().unwrap(), HandleKind::Eventfd);
    }
    assert_eq!(be.handles.kind(7), Some(HandleKind::Eventfd));
    let mut g = Guest::new(&mut be, h, false);
    g.client(
        &mut be,
        &[MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish()],
    );
    // The compositor offers linux-dmabuf; the client binds it and adds a
    // plane.
    use std::io::Write;
    (&server)
        .write_all(
            &MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
                .uint(5)
                .string(Some("zwp_linux_dmabuf_v1"))
                .uint(5)
                .finish(),
        )
        .unwrap();
    g.recv_until(&mut be, |b, _| !b.is_empty());
    let mut data = [
        MsgBuilder::new(2, op::wl_registry::REQ_BIND)
            .uint(5)
            .generic_new_id("zwp_linux_dmabuf_v1", 4, 3)
            .finish(),
        MsgBuilder::new(3, op::zwp_linux_dmabuf_v1::REQ_CREATE_PARAMS)
            .new_id(4)
            .finish(),
        MsgBuilder::new(4, op::zwp_linux_buffer_params_v1::REQ_ADD)
            .uint(0)
            .uint(0)
            .uint(256)
            .uint(0)
            .uint(0)
            .finish(),
    ]
    .concat();
    g.e.from_local(
        &mut data,
        &mut VecDeque::from([sys::memfd(c"guest-dmabuf", 0).unwrap()]),
        &mut GuestPlat,
    )
    .unwrap();
    g.flush(&mut be);
    server.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = Vec::new();
    let mut fds = Vec::new();
    while msgs(&bytes).len() < 3 {
        let mut b = [0u8; 4096];
        match sys::recv_with_fds(server.as_raw_fd(), &mut b, &mut fds) {
            Ok(n) => bytes.extend_from_slice(&b[..n]),
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
        assert!(Instant::now() < deadline);
    }
    // The add still carries one descriptor, so the compositor's fd queue
    // stays in step -- but not a dma-buf of anything.
    assert_eq!(fds.len(), 1);
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fds[0].as_raw_fd())).unwrap();
    assert!(link.to_string_lossy().starts_with("/memfd:"), "{link:?}");
}

#[test]
fn closing_a_wayland_handle_ends_its_connection() {
    let dir = tmpdir("serve-close");
    let sock = dir.join("wl");
    let l = UnixListener::bind(&sock).unwrap();
    let mut be = backend();
    be.set_wayland(Some(WlConfig::new(&sock)));
    hello(&mut be);
    let (_, h) = open(&mut be, frame::WL_OPEN_CONNECT);
    let (mut server, _) = l.accept().unwrap();
    assert!(be.wl_is_open(h));
    be.take_pump_cmds();
    assert_eq!(close(&mut be, h), 0);
    assert!(!be.wl_is_open(h));
    assert!(be.handles.kind(h).is_none());
    assert!(
        be.take_pump_cmds()
            .iter()
            .any(|c| matches!(c, PumpCmd::Unwatch { handle } if *handle == h))
    );
    // The compositor sees the client go.
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut b = [0u8; 64];
    assert_eq!(server.read(&mut b).unwrap(), 0);
    // And the handle is gone for every channel message.
    let r = recv(&mut be, h, MAX, HDR + MAX as usize);
    assert_eq!(status(&r), -libc::EBADF);
}

#[test]
fn a_fresh_hello_ends_every_wayland_connection() {
    let dir = tmpdir("serve-reset");
    let sock = dir.join("wl");
    let l = UnixListener::bind(&sock).unwrap();
    let mut be = backend();
    be.set_wayland(Some(WlConfig::new(&sock)));
    hello(&mut be);
    let (_, a) = open(&mut be, frame::WL_OPEN_CONNECT);
    let (_, b) = open(&mut be, frame::WL_OPEN_CONNECT);
    let mut servers: Vec<UnixStream> = (0..2).map(|_| l.accept().unwrap().0).collect();
    hello(&mut be);
    assert!(!be.wl_is_open(a) && !be.wl_is_open(b));
    assert_eq!(be.handle_count(), 0);
    for s in &mut servers {
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(s.read(&mut [0u8; 64]).unwrap(), 0);
    }
}

#[test]
fn a_channel_is_refused_before_hello_without_a_socket_or_in_an_unknown_mode() {
    let mut be = backend();
    // v1: nothing a v1 guest could use.
    assert_eq!(open(&mut be, frame::WL_OPEN_CONNECT).0, -libc::ENODEV);
    hello(&mut be);
    // No --wayland-socket, no --wayland-export.
    assert_eq!(open(&mut be, frame::WL_OPEN_CONNECT).0, -libc::ENODEV);
    assert_eq!(open(&mut be, frame::WL_OPEN_LISTEN).0, -libc::ENODEV);
    assert_eq!(open(&mut be, frame::WL_OPEN_ACCEPT).0, -libc::ENODEV);
    let dir = tmpdir("serve-refused");
    let sock = dir.join("wl");
    // Configured, but nobody listening: the connect error, not a handle.
    be.set_wayland(Some(WlConfig::new(&sock)));
    assert_eq!(open(&mut be, frame::WL_OPEN_CONNECT).0, -libc::ENOENT);
    assert_eq!(open(&mut be, 7).0, -libc::EINVAL);
    assert_eq!(be.handle_count(), 0);
    // WL_SEND and WL_RECV on a handle that is not a channel.
    let ev = be.adopt_for_test(crate::hostfd::new_eventfd().unwrap(), HandleKind::Eventfd);
    assert_eq!(
        status(&recv(&mut be, ev, MAX, HDR + MAX as usize)),
        -libc::EBADF
    );
    assert_eq!(
        status(&call(&mut be, MsgType::WlSend, ev, &[0u8; 16], 64)),
        -libc::EBADF
    );
}

#[test]
fn export_mode_listens_and_accepts_host_clients_as_channels() {
    let dir = tmpdir("serve-export");
    let path = dir.join("export-0");
    let (x, ready) = WlExport::bind(&path).unwrap();
    let mut be = backend();
    be.config_mut().wayland_export = Some(path.clone());
    be.set_wayland_export(Some((x.clone(), ready)));
    let req = HelloReq {
        proto: PROTO_V2,
        flags: HELLO_F_FRESH,
        ..Default::default()
    };
    let r = call(&mut be, MsgType::Hello, 0, bytes_of(&req), 4096);
    let resp: HelloResp = unsafe { (r[HDR..].as_ptr() as *const HelloResp).read_unaligned() };
    assert_eq!(resp.backend_caps & BCAP_WL_EXPORT, BCAP_WL_EXPORT);
    assert_eq!(resp.backend_caps & BCAP_WAYLAND, 0);

    be.take_pump_cmds();
    let (st, listen) = open(&mut be, frame::WL_OPEN_LISTEN);
    assert_eq!(st, 0);
    assert!(watched_legacy(&be.take_pump_cmds(), listen));
    // Nothing to accept yet, and nothing to send or receive on a listener.
    assert_eq!(open(&mut be, frame::WL_OPEN_ACCEPT).0, -libc::EAGAIN);
    assert_eq!(
        status(&recv(&mut be, listen, MAX, HDR + MAX as usize)),
        -libc::ENOTCONN
    );

    let _client = UnixStream::connect(&path).unwrap();
    let (fd, _) = be.handles.get(listen).unwrap();
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut p, 1, 2000) }, 1, "LISTEN readable");
    let (st, chan) = open(&mut be, frame::WL_OPEN_ACCEPT);
    assert_eq!(st, 0);
    assert!(be.wl_is_open(chan));
    // The accepted channel opens with the backend's HELLO like any other.
    let r = recv(&mut be, chan, MAX, HDR + MAX as usize);
    assert_eq!(status(&r), 0);
    let f = frame::decode(&r[HDR..]).unwrap();
    assert_eq!(f.records().next().map(|r| r.ty), Some(frame::REC_HELLO));
    x.shutdown();
}
