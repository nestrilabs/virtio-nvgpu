//! The host side against a fake compositor: a real socket, real threads, and
//! the guest's half played by a guest-side engine.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wlwire::engine::{Engine, EngineConfig, Local, Platform, Side};
use wlwire::frame::{self, Desc, DescOut};
use wlwire::policy::{LeaseGate, Policy};
use wlwire::proto::{self, Dir, iface, op};
use wlwire::sys;
use wlwire::wire::{self, MsgBuilder, Val, peek_header};

use super::conn::{HostFds, RecvOps, SendOps, WlConfig, WlConn, WlLimits, sock_fd};
use super::export::WlExport;
use crate::hostfd::HandleKind;

pub(super) fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nvwl-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Classifies memfds by name: "…ours…" is a lease of our GPU, "…dmabuf…" a
/// dma-buf, anything else Other.
pub(super) struct FakeHost;
impl HostFds for FakeHost {
    fn classify(&self, fd: BorrowedFd<'_>) -> HandleKind {
        let l = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap_or_default();
        let l = l.to_string_lossy();
        if l.contains("ours") {
            HandleKind::DrmLease(0)
        } else if l.contains("dmabuf") {
            HandleKind::Dmabuf
        } else {
            HandleKind::Other
        }
    }
}

#[derive(Default)]
struct Ops {
    exports: Vec<(u32, u32)>,
    adopted: Vec<u16>,
}
impl SendOps for Ops {
    fn prime_export(&mut self, owner: u32, gem: u32) -> std::io::Result<OwnedFd> {
        self.exports.push((owner, gem));
        sys::memfd(c"dmabuf-export", 4096)
    }
}
impl RecvOps for Ops {
    fn adopt(&mut self, _fd: OwnedFd, kind: u16) -> std::io::Result<(u32, u32)> {
        self.adopted.push(kind);
        Ok((100 + self.adopted.len() as u32, 4))
    }
}

/// The guest kernel, for the guest engine: DRM files arrive already adopted.
pub(super) struct GuestPlat;
impl Platform for GuestPlat {
    fn dmabuf_out(&mut self, _fd: OwnedFd) -> DescOut {
        DescOut::plain(Desc {
            a: 7,
            b: 42,
            ..Desc::new(frame::DESC_DMABUF)
        })
    }
    fn dmabuf_in(&mut self, _d: &Desc, fd: Option<OwnedFd>) -> std::io::Result<OwnedFd> {
        fd.ok_or_else(|| std::io::Error::other("none"))
    }
    fn drm_file_out(&mut self, _fd: OwnedFd) -> DescOut {
        DescOut::plain(Desc::invalid(frame::DESC_DRM_FILE))
    }
    fn drm_file_in(&mut self, _d: &Desc, _fd: Option<OwnedFd>) -> std::io::Result<OwnedFd> {
        sys::memfd(c"adopted", 0)
    }
}

/// The guest daemon's engine plus the channel calls, driven by hand.
struct Guest {
    e: Engine,
    conn: WlConn,
    ops: Ops,
}

impl Guest {
    fn new(conn: WlConn, drm_file: bool) -> Self {
        let mut e = Engine::new(EngineConfig {
            side: Side::Guest,
            local: Local::Client,
            policy: Policy {
                drm_file,
                lease: LeaseGate::Allow,
                fences: false,
            },
            rewrites: None,
            synth_released: true,
        });
        e.hello(if drm_file { frame::HELLO_G_DRM_FILE } else { 0 });
        let mut g = Guest {
            e,
            conn,
            ops: Ops::default(),
        };
        g.flush().unwrap();
        g
    }

    fn client(&mut self, msgs: &[Vec<u8>], fds: Vec<OwnedFd>) {
        let mut data = msgs.concat();
        let mut fds: VecDeque<OwnedFd> = fds.into();
        self.e
            .from_local(&mut data, &mut fds, &mut GuestPlat)
            .unwrap();
        self.flush().unwrap();
    }

    fn flush(&mut self) -> Result<(), i32> {
        let mut q = self.e.take_units();
        while !q.is_empty() {
            let (f, _) = frame::pack(&mut q, 1 << 20, 256, false);
            self.conn.send(&f, &mut self.ops)?;
        }
        Ok(())
    }

    /// WL_RECV until `pred` holds for what reached the client, or time out.
    fn recv_until(&mut self, pred: impl Fn(&[u8], usize) -> bool) -> (Vec<u8>, Vec<OwnedFd>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = Vec::new();
        let mut fds = Vec::new();
        loop {
            let f = self.conn.recv(1 << 20, 256, &mut self.ops).unwrap();
            let d = frame::decode(&f).unwrap();
            let n = d.descs.len();
            if !d.records.is_empty() {
                // The kernel would have adopted DRM files; here the guest
                // platform makes one per desc.
                self.e
                    .from_channel(&f, (0..n).map(|_| None).collect(), &mut GuestPlat)
                    .unwrap();
                for (b, fs) in self.e.local_out().drain() {
                    bytes.extend(b);
                    fds.extend(fs);
                }
            }
            if pred(&bytes, fds.len()) {
                return (bytes, fds);
            }
            assert!(
                Instant::now() < deadline,
                "timed out; have {} bytes",
                bytes.len()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

pub(super) fn msgs(mut b: &[u8]) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    while let Some(h) = peek_header(b) {
        v.push(b[..h.size as usize].to_vec());
        b = &b[h.size as usize..];
    }
    v
}

pub(super) fn global_names(b: &[u8]) -> Vec<String> {
    let d = &iface(proto::WL_REGISTRY).events[op::wl_registry::EVT_GLOBAL as usize];
    msgs(b)
        .iter()
        .filter(|m| peek_header(m).unwrap().object == 2)
        .filter_map(|m| match wire::parse(d, m).ok()?[1].val {
            Val::Str(Some(s)) => Some(String::from_utf8_lossy(s).into_owned()),
            _ => None,
        })
        .collect()
}

/// A compositor that answers get_registry with `globals`, sync with done, and
/// a lease-device bind with a drm_fd memfd named after the global (so
/// FakeHost can tell "ours" from not).
pub(super) fn fake_compositor(
    path: PathBuf,
    globals: Vec<(u32, &'static str, u32)>,
    lease_names: Vec<(u32, &'static str)>,
) {
    let l = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            let globals = globals.clone();
            let lease_names = lease_names.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = match s.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    while let Some(h) = peek_header(&buf) {
                        if buf.len() < h.size as usize {
                            break;
                        }
                        let m: Vec<u8> = buf.drain(..h.size as usize).collect();
                        let w = |x: &[u8]| u32::from_ne_bytes(x.try_into().unwrap());
                        match (h.object, h.opcode) {
                            (1, op::wl_display::REQ_GET_REGISTRY) => {
                                let r = w(&m[8..12]);
                                for (n, i, v) in &globals {
                                    let g = MsgBuilder::new(r, op::wl_registry::EVT_GLOBAL)
                                        .uint(*n)
                                        .string(Some(i))
                                        .uint(*v)
                                        .finish();
                                    s.write_all(&g).unwrap();
                                }
                            }
                            (1, op::wl_display::REQ_SYNC) => {
                                let cb = w(&m[8..12]);
                                s.write_all(
                                    &MsgBuilder::new(cb, op::wl_callback::EVT_DONE)
                                        .uint(0)
                                        .finish(),
                                )
                                .unwrap();
                                s.write_all(
                                    &MsgBuilder::new(1, op::wl_display::EVT_DELETE_ID)
                                        .uint(cb)
                                        .finish(),
                                )
                                .unwrap();
                            }
                            (_, op::wl_registry::REQ_BIND) if m.len() > 16 => {
                                let name = w(&m[8..12]);
                                let id = w(&m[m.len() - 4..]);
                                if let Some((_, tag)) = lease_names.iter().find(|(n, _)| *n == name)
                                {
                                    let c = std::ffi::CString::new(*tag).unwrap();
                                    let fd = sys::memfd(&c, 0).unwrap();
                                    let ev =
                                        MsgBuilder::new(id, op::wp_drm_lease_device_v1::EVT_DRM_FD)
                                            .finish();
                                    sys::send_with_fds(s.as_raw_fd(), &ev, &[fd.as_raw_fd()])
                                        .unwrap();
                                    let done =
                                        MsgBuilder::new(id, op::wp_drm_lease_device_v1::EVT_DONE)
                                            .finish();
                                    s.write_all(&done).unwrap();
                                }
                            }
                            _ => {}
                        }
                    }
                }
            });
        }
    });
}

pub(super) fn get_registry() -> Vec<Vec<u8>> {
    vec![
        MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish(),
        MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(3)
            .finish(),
    ]
}

pub(super) fn sync_done(b: &[u8], _: usize) -> bool {
    msgs(b).iter().any(|m| {
        let h = peek_header(m).unwrap();
        h.object == 3 && h.opcode == op::wl_callback::EVT_DONE
    })
}

#[test]
fn the_guest_sees_only_allowed_globals_at_clamped_versions() {
    let dir = tmpdir("reg");
    let sock = dir.join("wl");
    fake_compositor(
        sock.clone(),
        vec![
            (1, "wl_compositor", 6),
            (2, "zwlr_screencopy_manager_v1", 3),
            (3, "wp_security_context_manager_v1", 1),
        ],
        vec![],
    );
    let (conn, ready) = WlConn::open(&WlConfig::new(&sock), Arc::new(FakeHost)).unwrap();
    let mut g = Guest::new(conn, false);
    g.client(&get_registry(), vec![]);
    let (b, _) = g.recv_until(sync_done);
    assert_eq!(global_names(&b), vec!["wl_compositor"]);
    // Nothing more queued: the readiness eventfd is clear.
    let mut p = libc::pollfd {
        fd: ready.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut p, 1, 0) }, 0);
}

#[test]
fn a_guest_that_binds_a_hidden_global_loses_the_connection_and_is_told_why() {
    let dir = tmpdir("bind");
    let sock = dir.join("wl");
    fake_compositor(
        sock.clone(),
        vec![
            (1, "wl_compositor", 6),
            (2, "zwlr_screencopy_manager_v1", 3),
        ],
        vec![],
    );
    let (conn, _ready) = WlConn::open(&WlConfig::new(&sock), Arc::new(FakeHost)).unwrap();
    let mut g = Guest::new(conn, false);
    g.client(&get_registry(), vec![]);
    g.recv_until(sync_done);
    // Skip the guest engine (which would refuse): a raw frame straight to the
    // host, as a compromised guest kernel could send.
    let bind = MsgBuilder::new(2, op::wl_registry::REQ_BIND)
        .uint(2)
        .generic_new_id("zwlr_screencopy_manager_v1", 1, 9)
        .finish();
    let mut q = VecDeque::from([frame::Unit {
        rec: frame::record(frame::REC_WAYLAND, 0, 0, &bind),
        descs: vec![],
    }]);
    let (f, _) = frame::pack(&mut q, 1 << 20, 256, false);
    assert_eq!(
        g.conn.send(&f, &mut Ops::default()).unwrap_err(),
        libc::EPROTO
    );
    let f = g.conn.recv(1 << 20, 256, &mut Ops::default()).unwrap();
    let d = frame::decode(&f).unwrap();
    let types: Vec<u16> = d.records().map(|r| r.ty).collect();
    assert_eq!(types, vec![frame::REC_ERROR, frame::REC_HANGUP]);
    assert!(g.conn.is_closed());
    assert_eq!(
        g.conn.send(&f, &mut Ops::default()).unwrap_err(),
        libc::EPIPE
    );
}

#[test]
fn the_compositor_socket_is_drained_while_the_guest_is_not_reading() {
    let dir = tmpdir("drain");
    let sock = dir.join("wl");
    let l = UnixListener::bind(&sock).unwrap();
    let (conn, ready) = WlConn::open(&WlConfig::new(&sock), Arc::new(FakeHost)).unwrap();
    let (mut server, _) = l.accept().unwrap();
    let mut g = Guest::new(conn, false);
    g.client(
        &[MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish()],
        vec![],
    );
    // 3 MiB of events, far past any socket buffer, with the guest reading
    // nothing: a blocking write only completes if the backend reads eagerly.
    server
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let ev = MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
        .uint(1)
        .string(Some("wl_compositor"))
        .uint(6)
        .finish();
    let n = 3 * 1024 * 1024 / ev.len();
    let all = ev.repeat(n);
    server
        .write_all(&all)
        .expect("the compositor's writes must never block on us");
    let mut p = libc::pollfd {
        fd: ready.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut p, 1, 1000) }, 1);
    let (b, _) = g.recv_until(|b, _| b.len() >= all.len());
    assert_eq!(b.len(), all.len());
}

#[test]
fn wl_send_says_eagain_while_the_compositor_is_not_reading() {
    let dir = tmpdir("backlog");
    let sock = dir.join("wl");
    let l = UnixListener::bind(&sock).unwrap();
    let mut cfg = WlConfig::new(&sock);
    cfg.max_backlog = 64 * 1024;
    let (conn, _ready) = WlConn::open(&cfg, Arc::new(FakeHost)).unwrap();
    let (_server, _) = l.accept().unwrap(); // never reads
    let mut g = Guest::new(conn, false);
    let mut next = 3u32;
    let mut saw_eagain = false;
    for _ in 0..2000 {
        let batch: Vec<Vec<u8>> = (0..512)
            .map(|_| {
                next += 1;
                MsgBuilder::new(1, op::wl_display::REQ_SYNC)
                    .new_id(next)
                    .finish()
            })
            .collect();
        let mut data = batch.concat();
        g.e.from_local(&mut data, &mut VecDeque::new(), &mut GuestPlat)
            .unwrap();
        let mut q = g.e.take_units();
        let (f, _) = frame::pack(&mut q, 1 << 20, 256, false);
        match g.conn.send(&f, &mut g.ops) {
            Ok(r) => assert!(r.backlog as usize <= cfg.max_backlog + f.len() * 4),
            Err(e) => {
                assert_eq!(e, libc::EAGAIN);
                saw_eagain = true;
                break;
            }
        }
    }
    assert!(saw_eagain);
    let _ = sock_fd(&g.conn);
}

#[test]
fn a_dmabuf_plane_is_exported_on_its_owners_render_handle() {
    let dir = tmpdir("dmabuf");
    let sock = dir.join("wl");
    let l = UnixListener::bind(&sock).unwrap();
    let (conn, _ready) = WlConn::open(&WlConfig::new(&sock), Arc::new(FakeHost)).unwrap();
    let (server, _) = l.accept().unwrap();
    let mut g = Guest::new(conn, false);
    g.client(
        &[MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish()],
        vec![],
    );
    // The compositor offers linux-dmabuf.
    let mut srv = server.try_clone().unwrap();
    srv.write_all(
        &MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
            .uint(5)
            .string(Some("zwp_linux_dmabuf_v1"))
            .uint(5)
            .finish(),
    )
    .unwrap();
    g.recv_until(|b, _| !b.is_empty());
    g.client(
        &[
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
        ],
        vec![sys::memfd(c"guest-dmabuf", 0).unwrap()],
    );
    assert_eq!(g.ops.exports, vec![(7, 42)]);
    // The compositor receives the add with exactly one descriptor: the export.
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
    assert_eq!(fds.len(), 1);
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fds[0].as_raw_fd())).unwrap();
    assert!(link.to_string_lossy().contains("dmabuf-export"), "{link:?}");
}

#[test]
fn a_lease_device_is_offered_only_if_it_hands_out_files_of_our_gpu() {
    let dir = tmpdir("lease");
    let sock = dir.join("wl");
    fake_compositor(
        sock.clone(),
        vec![
            (40, "wp_drm_lease_device_v1", 1),
            (41, "wp_drm_lease_device_v1", 1),
            (1, "wl_compositor", 6),
        ],
        vec![(40, "lease-ours"), (41, "lease-other-gpu")],
    );
    let mut cfg = WlConfig::new(&sock);
    cfg.allow_lease = true;
    let (conn, _r) = WlConn::open(&cfg, Arc::new(FakeHost)).unwrap();
    let mut g = Guest::new(conn, true);
    g.client(&get_registry(), vec![]);
    let (b, _) = g.recv_until(sync_done);
    assert_eq!(
        global_names(&b),
        vec!["wp_drm_lease_device_v1", "wl_compositor"]
    );
    // Binding it delivers the drm_fd as a DRM_FILE the guest adopts.
    g.client(
        &[MsgBuilder::new(2, op::wl_registry::REQ_BIND)
            .uint(40)
            .generic_new_id("wp_drm_lease_device_v1", 1, 5)
            .finish()],
        vec![],
    );
    let (_, fds) = g.recv_until(|_, n| n >= 1);
    assert_eq!(fds.len(), 1);
    assert_eq!(g.ops.adopted, vec![frame::DESC_DRM_FILE]);
    // A guest that cannot adopt DRM files is never offered one.
    let (conn, _r) = WlConn::open(&cfg, Arc::new(FakeHost)).unwrap();
    let mut g = Guest::new(conn, false);
    g.client(&get_registry(), vec![]);
    let (b, _) = g.recv_until(sync_done);
    assert_eq!(global_names(&b), vec!["wl_compositor"]);
}

#[test]
fn the_export_socket_is_private_and_hands_over_connections() {
    let dir = tmpdir("export");
    let path = dir.join("export-0");
    let (x, ready) = WlExport::bind(&path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(x.accept_pending().is_none());
    let _c = UnixStream::connect(&path).unwrap();
    let mut p = libc::pollfd {
        fd: ready.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut p, 1, 2000) }, 1);
    let s = x.accept_pending().expect("our own uid is accepted");
    // The accepted connection becomes a channel facing a client.
    let (conn, _r) = WlConn::from_export(s, &WlConfig::new(&path), Arc::new(FakeHost)).unwrap();
    assert!(!conn.is_closed());
    x.shutdown();
    assert!(!path.exists());
}

#[test]
fn recv_refuses_a_buffer_too_small_for_a_record() {
    let dir = tmpdir("small");
    let sock = dir.join("wl");
    let _l = UnixListener::bind(&sock).unwrap();
    let (conn, _r) = WlConn::open(&WlConfig::new(&sock), Arc::new(FakeHost)).unwrap();
    assert_eq!(
        conn.recv(4096, 8, &mut Ops::default()).unwrap_err(),
        libc::EINVAL
    );
    let _ = Dir::Request;
}

/// Poll until `f` holds, or fail after 5 s.
fn until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn the_vms_queue_budget_drops_the_connection_that_passes_it_and_what_it_had_queued() {
    let dir = tmpdir("qbudget");
    let sock = dir.join("wl");
    let l = UnixListener::bind(&sock).unwrap();
    let mut cfg = WlConfig::new(&sock);
    // Far under the connection's own 64 MiB: the VM's budget is what trips.
    cfg.limits = WlLimits::new(64, 1 << 30, 256 * 1024);
    let (conn, _ready) = WlConn::open(&cfg, Arc::new(FakeHost)).unwrap();
    let (mut server, _) = l.accept().unwrap();
    let mut g = Guest::new(conn, false);
    g.client(
        &[MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish()],
        vec![],
    );
    let ev = MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
        .uint(1)
        .string(Some("wl_compositor"))
        .uint(6)
        .finish();
    server
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    // Four times the budget, with the guest reading nothing. The backend
    // shuts the socket part way through, so the write may fail.
    let _ = server.write_all(&ev.repeat((1 << 20) / ev.len()));
    until("the connection to drop", || g.conn.is_closed());
    // What it had queued went with it; only the HANGUP it is owed is left.
    assert!(cfg.limits.queue.used() < 64, "{}", cfg.limits.queue.used());
    let f = g.conn.recv(1 << 20, 256, &mut g.ops).unwrap();
    let types: Vec<u16> = frame::decode(&f).unwrap().records().map(|r| r.ty).collect();
    assert_eq!(types, vec![frame::REC_HANGUP]);
    assert_eq!(cfg.limits.queue.used(), 0);
}

#[test]
fn every_connection_gives_back_its_queued_bytes_when_it_goes() {
    let dir = tmpdir("qdrop");
    let sock = dir.join("wl");
    let _l = UnixListener::bind(&sock).unwrap();
    let cfg = WlConfig::new(&sock);
    let (conn, _ready) = WlConn::open(&cfg, Arc::new(FakeHost)).unwrap();
    // The backend's HELLO waits, unread, on the VM's budget.
    assert!(cfg.limits.queue.used() > 0);
    drop(conn);
    assert_eq!(cfg.limits.queue.used(), 0);
}

#[test]
fn a_connection_that_hangs_up_gives_its_shm_back_before_the_guest_closes_it() {
    let dir = tmpdir("shmback");
    let sock = dir.join("wl");
    let l = UnixListener::bind(&sock).unwrap();
    let cfg = WlConfig::new(&sock);
    let (conn, _ready) = WlConn::open(&cfg, Arc::new(FakeHost)).unwrap();
    let (server, _) = l.accept().unwrap();
    let mut g = Guest::new(conn, false);
    g.client(
        &[MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish()],
        vec![],
    );
    let mut srv = server.try_clone().unwrap();
    srv.write_all(
        &MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
            .uint(5)
            .string(Some("wl_shm"))
            .uint(1)
            .finish(),
    )
    .unwrap();
    g.recv_until(|b, _| !b.is_empty());
    g.client(
        &[
            MsgBuilder::new(2, op::wl_registry::REQ_BIND)
                .uint(5)
                .generic_new_id("wl_shm", 1, 3)
                .finish(),
            MsgBuilder::new(3, op::wl_shm::REQ_CREATE_POOL)
                .new_id(4)
                .int(65536)
                .finish(),
        ],
        vec![sys::memfd(c"guest-pool", 65536).unwrap()],
    );
    assert_eq!(cfg.limits.shm.used(), (65536, 1));
    // The compositor goes; the guest has not closed its handle.
    drop(srv);
    drop(server);
    until("the hangup", || g.conn.is_closed());
    assert_eq!(cfg.limits.shm.used(), (0, 0));
}
