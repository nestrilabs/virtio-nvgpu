//! End to end, minus the VM: real clients → the guest daemon → an in-process
//! channel → the backend's `WlConn` → a real headless compositor.
//!
//! The channel here is the backend's own connection object called directly,
//! with the kernel's two jobs (resolving a guest dma-buf, adopting a DRM file)
//! stubbed -- there is no GPU -- so everything between a client's socket and
//! the compositor's is the code that ships: both engines, the frame format,
//! the reader thread, stream pumping, shm copying, blobs, the allowlist.
//!
//! Each scenario also runs with the channel going through the backend's
//! dispatcher instead (`Via::Dispatcher`): OPEN(DEV_WAYLAND), WL_SEND and
//! WL_RECV as the guest kernel sends them, served by `NvidiaBackend` with its
//! handle table, response sizing and pump watch -- the path a VM takes, minus
//! the virtqueue.
//!
//! Needs sway, wayland-info, wl-clipboard and weston's demo clients on PATH,
//! so it is ignored by default; `scripts/wl-loopback-test.sh` provides them
//! with nix and runs it.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use device::hostfd::HandleKind;
use device::nvidia::NvidiaBackend;
use device::pump::PumpCmd;
use device::shm::ZoneConfig;
use device::wl::{HostFds, RecvOps, SendOps, WlConfig, WlConn};
use nvgpu_wl_guest::channel::{Channel, Connector, HostInfo, Received, Sent};
use nvgpu_wl_guest::daemon::{Config, Daemon, Totals};
use nvgpu_wl_guest::uapi;
use protocol::messages::{DEV_WAYLAND, HELLO_F_FRESH, MsgType, PROTO_V2};
use wlwire::frame::{self, Desc};
use wlwire::proto::op;
use wlwire::sys;
use wlwire::wire::{MsgBuilder, peek_header};

// ───────────────────────── the in-process channel ─────────────────────────

struct Classify;
impl HostFds for Classify {
    fn classify(&self, fd: BorrowedFd<'_>) -> HandleKind {
        let l = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap_or_default();
        if l.to_string_lossy().contains("dmabuf") {
            HandleKind::Dmabuf
        } else {
            HandleKind::Other
        }
    }
}

/// No GPU: nothing to export, nothing to adopt.
struct NoGpu;
impl SendOps for NoGpu {
    fn prime_export(&mut self, _owner: u32, _gem: u32) -> io::Result<OwnedFd> {
        Err(io::Error::other("no GPU in this test"))
    }
}
impl RecvOps for NoGpu {
    fn adopt(&mut self, _fd: OwnedFd, _kind: u16) -> io::Result<(u32, u32)> {
        Err(io::Error::other("no GPU in this test"))
    }
}

struct LoopChannel {
    conn: WlConn,
    ready: OwnedFd,
}

impl Channel for LoopChannel {
    fn send(&mut self, f: &mut [u8], fds: &[Option<OwnedFd>]) -> io::Result<Sent> {
        mark_unresolvable(f, fds);
        match self.conn.send(f, &mut NoGpu) {
            Ok(r) => Ok(Sent::Accepted { backlog: r.backlog }),
            Err(libc::EAGAIN) => Ok(Sent::Busy),
            Err(e) => Err(io::Error::from_raw_os_error(e)),
        }
    }
    fn recv(
        &mut self,
        max: usize,
        _card: Option<RawFd>,
        _render: Option<RawFd>,
    ) -> io::Result<Received> {
        let f = self
            .conn
            .recv(max as u32, frame::MAX_DESC as u32, &mut NoGpu)
            .map_err(io::Error::from_raw_os_error)?;
        let d = frame::decode(&f).unwrap();
        let more = d.flags & frame::FRAME_F_MORE != 0;
        let fds = (0..d.descs.len()).map(|_| None).collect();
        Ok(Received {
            frame: f,
            fds,
            more,
        })
    }
    fn poll_fd(&self) -> RawFd {
        self.ready.as_raw_fd()
    }
}

struct LoopConnector {
    cfg: WlConfig,
}

impl Connector for LoopConnector {
    fn info(&mut self) -> io::Result<HostInfo> {
        Ok(HostInfo {
            caps: uapi::CAP_WAYLAND,
            clock_offset_ns: 0,
            max_frame: 256 * 1024,
            devmap: Vec::new(),
        })
    }
    fn connect(&mut self, mode: u32) -> io::Result<Box<dyn Channel>> {
        assert_eq!(mode, uapi::CONNECT);
        let (conn, ready) = WlConn::open(&self.cfg, Arc::new(Classify))?;
        Ok(Box::new(LoopChannel { conn, ready }))
    }
}

// ─────────────────── the channel through the dispatcher ───────────────────

/// The backend as the transport drives it, one message at a time.
struct Dispatcher {
    be: Mutex<NvidiaBackend>,
}

const HDR: usize = 16;

impl Dispatcher {
    fn new(host_socket: &Path) -> Arc<Self> {
        let mut be = NvidiaBackend::new(ZoneConfig {
            uc_size: 4096,
            wc_size: 4096,
            wb_size: 4096,
        });
        be.set_wayland(Some(WlConfig::new(host_socket)));
        let d = Arc::new(Dispatcher { be: Mutex::new(be) });
        let mut hello = Vec::new();
        for w in [PROTO_V2, HELLO_F_FRESH, 0, 0] {
            hello.extend_from_slice(&w.to_le_bytes());
        }
        let (st, _, _) = d.call(MsgType::Hello, 0, &hello, 64);
        assert_eq!(st, 0, "HELLO");
        d
    }

    /// (status, header handle, payload) of one message answered into `cap`
    /// bytes.
    fn call(&self, t: MsgType, handle: u32, body: &[u8], cap: usize) -> (i32, u32, Vec<u8>) {
        let mut req = Vec::with_capacity(HDR + body.len());
        for w in [t as u32, handle, 0, 1] {
            req.extend_from_slice(&w.to_le_bytes());
        }
        req.extend_from_slice(body);
        let mut resp = vec![0u8; cap];
        let n = self.be.lock().unwrap().dispatch(&req, &mut resp);
        assert!(n >= HDR, "a reply without a header");
        let w = |i: usize| u32::from_le_bytes(resp[i..i + 4].try_into().unwrap());
        (w(8) as i32, w(4), resp[HDR..n].to_vec())
    }
}

struct DispatchChannel {
    d: Arc<Dispatcher>,
    handle: u32,
    /// The pump's copy of the channel's readiness eventfd, from the watch the
    /// OPEN asked for: readable exactly when the event queue would say so.
    ready: OwnedFd,
}

impl Channel for DispatchChannel {
    fn send(&mut self, f: &mut [u8], fds: &[Option<OwnedFd>]) -> io::Result<Sent> {
        mark_unresolvable(f, fds);
        match self.d.call(MsgType::WlSend, self.handle, f, 64) {
            (0, _, p) => Ok(Sent::Accepted {
                backlog: u32::from_le_bytes(p[4..8].try_into().unwrap()),
            }),
            (e, _, _) if e == -libc::EAGAIN => Ok(Sent::Busy),
            (e, _, _) => Err(io::Error::from_raw_os_error(-e)),
        }
    }
    fn recv(
        &mut self,
        max: usize,
        _card: Option<RawFd>,
        _render: Option<RawFd>,
    ) -> io::Result<Received> {
        // What nvgpu_wl.c posts: the header and the whole frame it asks for.
        let mut body = Vec::new();
        body.extend_from_slice(&(max as u32).to_le_bytes());
        body.extend_from_slice(&(frame::MAX_DESC as u32).to_le_bytes());
        let (st, _, f) = self.d.call(MsgType::WlRecv, self.handle, &body, HDR + max);
        if st < 0 {
            return Err(io::Error::from_raw_os_error(-st));
        }
        let d = frame::decode(&f).map_err(|e| io::Error::other(format!("{e:?}")))?;
        let more = d.flags & frame::FRAME_F_MORE != 0;
        let fds = (0..d.descs.len()).map(|_| None).collect();
        Ok(Received {
            frame: f,
            fds,
            more,
        })
    }
    fn poll_fd(&self) -> RawFd {
        self.ready.as_raw_fd()
    }
}

impl Drop for DispatchChannel {
    fn drop(&mut self) {
        let (st, _, _) = self.d.call(MsgType::Close, self.handle, &[], 64);
        assert_eq!(st, 0, "CLOSE of a channel");
    }
}

struct DispatchConnector {
    d: Arc<Dispatcher>,
}

impl Connector for DispatchConnector {
    fn info(&mut self) -> io::Result<HostInfo> {
        Ok(HostInfo {
            caps: uapi::CAP_WAYLAND,
            clock_offset_ns: 0,
            max_frame: 256 * 1024 - HDR,
            devmap: Vec::new(),
        })
    }
    fn connect(&mut self, mode: u32) -> io::Result<Box<dyn Channel>> {
        let mut open = Vec::new();
        open.extend_from_slice(&DEV_WAYLAND.to_le_bytes());
        open.extend_from_slice(&mode.to_le_bytes());
        let (st, handle, _) = self.d.call(MsgType::Open, 0, &open, 64);
        if st < 0 {
            return Err(io::Error::from_raw_os_error(-st));
        }
        let cmds = self.d.be.lock().unwrap().take_pump_cmds();
        let ready = cmds
            .into_iter()
            .find_map(|c| match c {
                PumpCmd::Watch { handle: h, fd, .. } if h == handle => Some(fd),
                _ => None,
            })
            .expect("OPEN(DEV_WAYLAND) watches its handle");
        Ok(Box::new(DispatchChannel {
            d: self.d.clone(),
            handle,
            ready,
        }))
    }
}

/// The kernel's SEND without a GPU: a dma-buf it cannot resolve goes as
/// invalid.
fn mark_unresolvable(f: &mut [u8], fds: &[Option<OwnedFd>]) {
    for (i, fd) in fds.iter().enumerate() {
        if fd.is_some() {
            let at = frame::FRAME_HDR_LEN + i * frame::DESC_LEN;
            let mut d = Desc::read(&f[at..at + frame::DESC_LEN]);
            d.flags |= frame::DESC_F_INVALID;
            let mut b = Vec::new();
            d.write(&mut b);
            f[at..at + frame::DESC_LEN].copy_from_slice(&b);
        }
    }
}

/// Which channel the daemon gets.
#[derive(Clone, Copy, Debug)]
enum Via {
    /// The backend's connection object, called directly.
    Conn,
    /// The backend's message dispatcher, as the transport calls it.
    Dispatcher,
}

// ───────────────────────── harness ─────────────────────────

fn tool(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for d in std::env::split_paths(&path) {
        let p = d.join(name);
        if p.is_file() {
            return p;
        }
    }
    panic!("{name} is not on PATH (run scripts/wl-loopback-test.sh)");
}

/// A short private directory: socket paths are limited to 108 bytes.
fn runtime_dir(tag: &str) -> PathBuf {
    let base = std::env::var_os("NVWL_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let d = base.join(format!("nvwl{}{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_sway(dir: &Path) -> (Kill, PathBuf) {
    let cfg = dir.join("sway.cfg");
    std::fs::write(&cfg, "output HEADLESS-1 resolution 800x600\n").unwrap();
    let log = std::fs::File::create(dir.join("sway.log")).unwrap();
    let child = Command::new(tool("sway"))
        .arg("-c")
        .arg(&cfg)
        .env("XDG_RUNTIME_DIR", dir)
        .env("WLR_BACKENDS", "headless")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env("WLR_RENDERER", "pixman")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                let n = p.file_name().unwrap().to_string_lossy().into_owned();
                n.starts_with("wayland-") && !n.ends_with(".lock")
            });
        if let Some(p) = found {
            return (Kill(child), p);
        }
        assert!(
            Instant::now() < deadline,
            "sway did not start; see {}",
            dir.join("sway.log").display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct DaemonRun {
    stop: Arc<AtomicBool>,
    totals: Arc<Mutex<Totals>>,
    thread: Option<std::thread::JoinHandle<()>>,
    socket: PathBuf,
}

impl DaemonRun {
    fn start(dir: &Path, host_socket: &Path, via: Via) -> DaemonRun {
        let connector: Box<dyn Connector + Send> = match via {
            Via::Conn => Box::new(LoopConnector {
                cfg: WlConfig::new(host_socket),
            }),
            Via::Dispatcher => Box::new(DispatchConnector {
                d: Dispatcher::new(host_socket),
            }),
        };
        Self::with(dir, connector)
    }

    fn with(dir: &Path, connector: Box<dyn Connector + Send>) -> DaemonRun {
        let socket = dir.join("proxy-0");
        let cfg = Config::new(&socket);
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut d = Daemon::new(cfg, connector).unwrap();
            tx.send((d.stop_flag(), d.totals())).unwrap();
            d.run().unwrap();
        });
        let (stop, totals) = rx.recv().unwrap();
        DaemonRun {
            stop,
            totals,
            thread: Some(thread),
            socket,
        }
    }
    fn totals(&self) -> Totals {
        self.totals.lock().unwrap().clone()
    }
}

impl Drop for DaemonRun {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn client(prog: &str, args: &[&str], display: &Path, dir: &Path) -> Command {
    let mut c = Command::new(tool(prog));
    c.args(args)
        .env("WAYLAND_DISPLAY", display)
        .env("XDG_RUNTIME_DIR", dir);
    c
}

/// Run to completion with a deadline; (success, stdout, stderr).
fn run(mut c: Command, secs: u64) -> (bool, String, String) {
    let mut child = c
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            let mut out = String::new();
            let mut err = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut out)
                .unwrap();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            return (st.success(), out, err);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            let mut err = String::new();
            let _ = child.stderr.take().unwrap().read_to_string(&mut err);
            return (false, String::new(), format!("timed out; stderr: {err}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A keyboard for the headless seat, straight on the compositor (not through
/// the proxy): a virtual keyboard with a keymap, held while the connection
/// lives. Without one the seat has no keyboard, clients get no keymap, and
/// nothing can take focus.
fn virtual_keyboard(sock: &Path) -> UnixStream {
    let mut s = UnixStream::connect(sock).unwrap();
    s.write_all(
        &MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish(),
    )
    .unwrap();
    s.write_all(
        &MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(3)
            .finish(),
    )
    .unwrap();
    let (mut seat, mut vk) = (None, None);
    let mut buf = Vec::new();
    'outer: loop {
        let mut chunk = [0u8; 8192];
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0);
        buf.extend_from_slice(&chunk[..n]);
        while let Some(h) = peek_header(&buf) {
            if buf.len() < h.size as usize {
                break;
            }
            let m: Vec<u8> = buf.drain(..h.size as usize).collect();
            if h.object == 3 {
                break 'outer;
            }
            if h.object == 2 && h.opcode == op::wl_registry::EVT_GLOBAL {
                let name = u32::from_ne_bytes(m[8..12].try_into().unwrap());
                let len = u32::from_ne_bytes(m[12..16].try_into().unwrap()) as usize;
                let iface = std::str::from_utf8(&m[16..16 + len - 1])
                    .unwrap()
                    .to_string();
                match iface.as_str() {
                    "wl_seat" => seat = Some(name),
                    "zwp_virtual_keyboard_manager_v1" => vk = Some(name),
                    _ => {}
                }
            }
        }
    }
    let bind = |name, iface: &str, id| {
        MsgBuilder::new(2, op::wl_registry::REQ_BIND)
            .uint(name)
            .generic_new_id(iface, 1, id)
            .finish()
    };
    s.write_all(&bind(seat.unwrap(), "wl_seat", 4)).unwrap();
    s.write_all(&bind(
        vk.expect("the compositor has no virtual keyboard"),
        "zwp_virtual_keyboard_manager_v1",
        5,
    ))
    .unwrap();
    // zwp_virtual_keyboard_manager_v1.create_virtual_keyboard(seat, id)
    s.write_all(&MsgBuilder::new(5, 0).object(4).new_id(6).finish())
        .unwrap();
    let keymap = b"xkb_keymap {\n xkb_keycodes { include \"evdev+aliases(qwerty)\" };\n \
        xkb_types { include \"complete\" };\n xkb_compat { include \"complete\" };\n \
        xkb_symbols { include \"pc+us+inet(evdev)\" };\n};\n\0";
    let fd = sys::memfd(c"keymap", keymap.len() as u64).unwrap();
    sys::pwrite_full(fd.as_raw_fd(), keymap, 0).unwrap();
    // zwp_virtual_keyboard_v1.keymap(format = XKB_V1, fd, size)
    let km = MsgBuilder::new(6, 0)
        .uint(1)
        .uint(keymap.len() as u32)
        .finish();
    sys::send_with_fds(s.as_raw_fd(), &km, &[fd.as_raw_fd()]).unwrap();
    s.write_all(
        &MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(7)
            .finish(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    s
}

fn settle(d: &DaemonRun, pred: impl Fn(&Totals) -> bool) -> Totals {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let t = d.totals();
        if pred(&t) || Instant::now() > deadline {
            return t;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "needs sway, wayland-utils, wl-clipboard and weston on PATH; run scripts/wl-loopback-test.sh"]
fn real_clients_run_through_the_proxy_against_a_real_compositor() {
    against_sway(Via::Conn, "s");
}

#[test]
#[ignore = "needs sway, wayland-utils, wl-clipboard and weston on PATH; run scripts/wl-loopback-test.sh"]
fn real_clients_run_through_the_backend_dispatcher_against_sway() {
    against_sway(Via::Dispatcher, "S");
}

fn against_sway(via: Via, tag: &str) {
    let dir = runtime_dir(tag);
    let (_sway, host_sock) = start_sway(&dir);
    let _kbd = virtual_keyboard(&host_sock);
    let d = DaemonRun::start(&dir, &host_sock, via);
    let proxy = d.socket.clone();

    // ── the registry: filtered, and a client that walks all of it works ──
    let (ok, direct, _) = run(client("wayland-info", &[], &host_sock, &dir), 10);
    assert!(ok);
    let (ok, proxied, err) = run(client("wayland-info", &[], &proxy, &dir), 10);
    assert!(ok, "wayland-info through the proxy failed: {err}");
    for allowed in [
        "wl_compositor",
        "wl_shm",
        "xdg_wm_base",
        "wl_seat",
        "wl_output",
        "wp_presentation",
        "wp_viewporter",
    ] {
        assert!(
            proxied.contains(&format!("'{allowed}'")),
            "{allowed} missing through the proxy:\n{proxied}"
        );
    }
    for hidden in [
        "zwlr_data_control_manager_v1",
        "ext_data_control_manager_v1",
        "wp_security_context_manager_v1",
        "zwp_virtual_keyboard_manager_v1",
        "zwlr_virtual_pointer_manager_v1",
        "zwp_input_method_manager_v2",
        "zwlr_screencopy_manager_v1",
        "ext_image_copy_capture_manager_v1",
        "zwlr_gamma_control_manager_v1",
        "zwlr_layer_shell_v1",
        "ext_session_lock_manager_v1",
        "zwlr_output_manager_v1",
    ] {
        assert!(
            direct.contains(hidden),
            "the compositor should offer {hidden} directly"
        );
        assert!(
            !proxied.contains(hidden),
            "{hidden} leaked through the proxy"
        );
    }
    // The seat's keyboard came through, keymap and all (a blob).
    assert!(
        proxied.contains("keyboard"),
        "no keyboard through the proxy:\n{proxied}"
    );
    let t = settle(&d, |t| t.blobs_received >= 1);
    assert!(t.blobs_received >= 1, "no keymap blob: {t:?}");
    println!(
        "wayland-info: {} lines direct, {} through the proxy",
        direct.lines().count(),
        proxied.lines().count()
    );

    // ── an shm client drawing frames, with presentation feedback ──
    let mut shm = client("weston-presentation-shm", &["-f"], &proxy, &dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        shm.try_wait().unwrap().is_none(),
        "weston-presentation-shm exited early"
    );
    let _ = shm.kill();
    let _ = shm.wait();
    let mut out = String::new();
    shm.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    let t = settle(&d, |t| t.clients >= 3 && t.commits > 0);
    println!(
        "presentation-shm: {} commits, {} shm syncs ({} bytes), {} timestamps translated, {} feedback lines",
        t.commits,
        t.shm_syncs,
        t.shm_bytes,
        t.time_rewrites,
        out.lines().count()
    );
    assert!(t.commits >= 20, "too few commits: {t:?}");
    assert!(
        t.shm_syncs >= 20 && t.shm_bytes > 0,
        "shm contents did not reach the host: {t:?}"
    );
    assert!(
        t.time_rewrites >= 10,
        "presentation feedback did not come back: {t:?}"
    );

    // ── the clipboard, host → guest: a stream through both engines ──
    let host_copy = Kill(
        client(
            "wl-copy",
            &["--foreground", "hello from the host"],
            &host_sock,
            &dir,
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(300));
    let (ok, pasted, err) = run(client("wl-paste", &["-n"], &proxy, &dir), 10);
    assert!(ok, "wl-paste through the proxy failed: {err}");
    assert_eq!(pasted, "hello from the host");
    drop(host_copy);

    // ── and guest → host ──
    let guest_copy = Kill(
        client(
            "wl-copy",
            &["--foreground", "hello from the guest"],
            &proxy,
            &dir,
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let pasted = loop {
        let (_, p, _) = run(client("wl-paste", &["-n"], &host_sock, &dir), 5);
        if p == "hello from the guest" || Instant::now() > deadline {
            break p;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(pasted, "hello from the guest");
    drop(guest_copy);
    let t = settle(&d, |t| t.streams >= 2);
    println!(
        "clipboard: {} streams, {} bytes in, {} bytes out",
        t.streams, t.stream_bytes_in, t.stream_bytes_out
    );
    assert!(t.streams >= 2, "{t:?}");
    let t = d.totals();
    assert!(t.errors.is_empty(), "protocol errors: {:?}", t.errors);
    println!("totals: {t:?}");
}

fn start_weston(dir: &Path) -> (Kill, PathBuf) {
    let log = std::fs::File::create(dir.join("weston.log")).unwrap();
    let child = Command::new(tool("weston"))
        .args(["--backend=headless", "--socket=wayland-w", "--idle-time=0"])
        .env("XDG_RUNTIME_DIR", dir)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let sock = dir.join("wayland-w");
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&sock).is_err() {
        assert!(
            Instant::now() < deadline,
            "weston did not start; see {}",
            dir.join("weston.log").display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (Kill(child), sock)
}

/// The same proxy in front of a different compositor: weston's headless
/// backend (no seat, a CLOCK_MONOTONIC_RAW presentation clock).
#[test]
#[ignore = "needs weston and wayland-utils on PATH; run scripts/wl-loopback-test.sh"]
fn real_clients_run_through_the_proxy_against_weston() {
    against_weston(Via::Conn, "w");
}

#[test]
#[ignore = "needs weston and wayland-utils on PATH; run scripts/wl-loopback-test.sh"]
fn real_clients_run_through_the_backend_dispatcher_against_weston() {
    against_weston(Via::Dispatcher, "W");
}

fn against_weston(via: Via, tag: &str) {
    let dir = runtime_dir(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let (_weston, host_sock) = start_weston(&dir);
    let d = DaemonRun::start(&dir, &host_sock, via);
    let proxy = d.socket.clone();
    let (ok, proxied, err) = run(client("wayland-info", &[], &proxy, &dir), 10);
    assert!(ok, "wayland-info through the proxy failed: {err}");
    assert!(proxied.contains("'wl_compositor'") && proxied.contains("'xdg_wm_base'"));
    // weston's own globals, none of which is on the allowlist.
    for hidden in [
        "weston_capture_v1",
        "weston_desktop_shell",
        "zwp_input_panel_v1",
        "zwp_linux_explicit_synchronization_v1",
    ] {
        assert!(
            !proxied.contains(hidden),
            "{hidden} leaked through the proxy"
        );
    }
    let mut shm = client("weston-presentation-shm", &["-f"], &proxy, &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        shm.try_wait().unwrap().is_none(),
        "weston-presentation-shm exited early"
    );
    let _ = shm.kill();
    let _ = shm.wait();
    let t = settle(&d, |t| t.clients >= 2 && t.commits > 0);
    println!("weston: {t:?}");
    assert!(t.commits >= 20 && t.shm_syncs >= 20, "{t:?}");
    assert!(t.time_rewrites >= 10, "{t:?}");
    assert!(t.errors.is_empty(), "protocol errors: {:?}", t.errors);
}

// ───────────────────── explicit sync, at the codec level ─────────────────────
//
// No GPU and no compositor with explicit sync: the syncobjs are memfds
// standing in for them, the guest kernel's SEND is a table from a guest file
// to a backend handle, and the compositor is a few lines of wire protocol
// that offers `wp_linux_drm_syncobj_manager_v1` and reports what
// `import_timeline` brought it. Everything between -- both engines, the
// allowlist and its HELLO gate, the frame's SYNCOBJ descriptor, WlConn and
// its SendOps -- is the code that ships.

/// The guest kernel's half: a syncobj file it knows (by inode, as
/// nvgpu_hostfile_fget knows its own files) is sent as its backend handle;
/// anything else as invalid.
struct SyncobjKernel {
    guest: HashMap<u64, u32>,
}

impl SyncobjKernel {
    fn resolve(&self, f: &mut [u8], fds: &[Option<OwnedFd>]) {
        for (i, fd) in fds.iter().enumerate() {
            let Some(fd) = fd else { continue };
            let at = frame::FRAME_HDR_LEN + i * frame::DESC_LEN;
            let mut d = Desc::read(&f[at..at + frame::DESC_LEN]);
            let ino = sys::fstat(fd.as_raw_fd()).unwrap().st_ino;
            match (d.kind, self.guest.get(&ino)) {
                (frame::DESC_SYNCOBJ, Some(&h)) => d.a = h,
                _ => d.flags |= frame::DESC_F_INVALID,
            }
            d.fd = -1;
            let mut b = Vec::new();
            d.write(&mut b);
            f[at..at + frame::DESC_LEN].copy_from_slice(&b);
        }
    }
}

/// The backend's handle table, as far as SendOps::syncobj looks at it.
struct HostSyncobjs(HashMap<u32, OwnedFd>);

impl SendOps for HostSyncobjs {
    fn prime_export(&mut self, _owner: u32, _gem: u32) -> io::Result<OwnedFd> {
        Err(io::Error::other("no GPU in this test"))
    }
    fn syncobj(&mut self, handle: u32) -> io::Result<OwnedFd> {
        match self.0.get(&handle) {
            Some(fd) => fd.try_clone(),
            None => Err(io::Error::from_raw_os_error(libc::EBADF)),
        }
    }
}

struct SyncobjChannel {
    conn: WlConn,
    ready: OwnedFd,
    kernel: Arc<SyncobjKernel>,
    host: HostSyncobjs,
}

impl Channel for SyncobjChannel {
    fn send(&mut self, f: &mut [u8], fds: &[Option<OwnedFd>]) -> io::Result<Sent> {
        self.kernel.resolve(f, fds);
        match self.conn.send(f, &mut self.host) {
            Ok(r) => Ok(Sent::Accepted { backlog: r.backlog }),
            Err(libc::EAGAIN) => Ok(Sent::Busy),
            Err(e) => Err(io::Error::from_raw_os_error(e)),
        }
    }
    fn recv(
        &mut self,
        max: usize,
        _card: Option<RawFd>,
        _render: Option<RawFd>,
    ) -> io::Result<Received> {
        let f = self
            .conn
            .recv(max as u32, frame::MAX_DESC as u32, &mut NoGpu)
            .map_err(io::Error::from_raw_os_error)?;
        let d = frame::decode(&f).unwrap();
        let more = d.flags & frame::FRAME_F_MORE != 0;
        let fds = (0..d.descs.len()).map(|_| None).collect();
        Ok(Received {
            frame: f,
            fds,
            more,
        })
    }
    fn poll_fd(&self) -> RawFd {
        self.ready.as_raw_fd()
    }
}

struct SyncobjConnector {
    cfg: WlConfig,
    caps: u32,
    kernel: Arc<SyncobjKernel>,
    /// Backend handle -> the host syncobj (a memfd) behind it.
    host: Vec<(u32, OwnedFd)>,
}

impl Connector for SyncobjConnector {
    fn info(&mut self) -> io::Result<HostInfo> {
        Ok(HostInfo {
            caps: self.caps,
            clock_offset_ns: 0,
            max_frame: 256 * 1024,
            devmap: Vec::new(),
        })
    }
    fn connect(&mut self, mode: u32) -> io::Result<Box<dyn Channel>> {
        assert_eq!(mode, uapi::CONNECT);
        let (conn, ready) = WlConn::open(&self.cfg, Arc::new(Classify))?;
        let host = self
            .host
            .iter()
            .map(|(h, fd)| (*h, fd.try_clone().unwrap()))
            .collect();
        Ok(Box::new(SyncobjChannel {
            conn,
            ready,
            kernel: self.kernel.clone(),
            host: HostSyncobjs(host),
        }))
    }
}

/// Read whole messages (and the descriptors that came with them) until
/// `f` says stop.
fn read_msgs(
    s: &UnixStream,
    buf: &mut Vec<u8>,
    fds: &mut Vec<OwnedFd>,
    mut f: impl FnMut(u32, u16, &[u8], &mut Vec<OwnedFd>) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        while let Some(h) = peek_header(buf) {
            if buf.len() < h.size as usize {
                break;
            }
            let m: Vec<u8> = buf.drain(..h.size as usize).collect();
            if f(h.object, h.opcode, &m, fds) {
                return;
            }
        }
        assert!(Instant::now() < deadline, "timed out on the wire");
        let mut chunk = [0u8; 4096];
        // recv_with_fds never blocks.
        match sys::recv_with_fds(s.as_raw_fd(), &mut chunk, fds) {
            Ok(n) => {
                assert!(n > 0, "the peer hung up");
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("recv: {e}"),
        }
    }
}

fn word(m: &[u8], i: usize) -> u32 {
    u32::from_ne_bytes(m[4 * i..4 * i + 4].try_into().unwrap())
}

/// A compositor offering only `wp_linux_drm_syncobj_manager_v1`: answers
/// get_registry and sync, and sends every descriptor an import_timeline
/// brought down `got`.
fn syncobj_compositor(sock: PathBuf, got: mpsc::Sender<OwnedFd>) {
    let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let (s, _) = l.accept().unwrap();
        let (mut buf, mut fds) = (Vec::new(), Vec::new());
        let mut manager = None;
        let mut out = s.try_clone().unwrap();
        read_msgs(&s, &mut buf, &mut fds, |obj, opc, m, fds| {
            match (obj, opc) {
                (1, op::wl_display::REQ_GET_REGISTRY) => {
                    let reg = word(m, 2);
                    let g = MsgBuilder::new(reg, op::wl_registry::EVT_GLOBAL)
                        .uint(1)
                        .string(Some("wp_linux_drm_syncobj_manager_v1"))
                        .uint(1)
                        .finish();
                    out.write_all(&g).unwrap();
                }
                (1, op::wl_display::REQ_SYNC) => {
                    let cb = word(m, 2);
                    out.write_all(
                        &MsgBuilder::new(cb, op::wl_callback::EVT_DONE)
                            .uint(0)
                            .finish(),
                    )
                    .unwrap();
                    out.write_all(
                        &MsgBuilder::new(1, op::wl_display::EVT_DELETE_ID)
                            .uint(cb)
                            .finish(),
                    )
                    .unwrap();
                }
                // bind(name, interface, version, id): the id is last.
                (_, op::wl_registry::REQ_BIND) if manager.is_none() && obj != 1 => {
                    manager = Some(word(m, m.len() / 4 - 1));
                }
                (o, op::wp_linux_drm_syncobj_manager_v1::REQ_IMPORT_TIMELINE)
                    if Some(o) == manager =>
                {
                    assert!(!fds.is_empty(), "import_timeline without its fd");
                    if got.send(fds.remove(0)).is_err() {
                        return true;
                    }
                }
                _ => {}
            }
            false
        });
    });
}

/// What a guest client saw and what the compositor got: whether the syncobj
/// global was offered, and the descriptor import_timeline delivered (if the
/// client could send one), for a guest whose kernel does or does not say
/// NVGPU_WL_CAP_SYNCOBJ, a backend that does or does not serve fences, and a
/// client syncobj the kernel does or does not know.
fn syncobj_round_trip(
    tag: &str,
    guest_caps: u32,
    backend_fences: bool,
    known: bool,
) -> (bool, Option<OwnedFd>, u64) {
    let dir = runtime_dir(tag);
    let host_sock = dir.join("host-0");
    let (tx, rx) = mpsc::channel();
    syncobj_compositor(host_sock.clone(), tx);

    // The client's syncobj, the guest kernel's handle for it, and the host
    // syncobj behind that handle.
    let guest_so = sys::memfd(c"guest-syncobj", 0).unwrap();
    let host_so = sys::memfd(c"host-syncobj", 0).unwrap();
    let host_ino = sys::fstat(host_so.as_raw_fd()).unwrap().st_ino;
    let mut guest = HashMap::new();
    if known {
        guest.insert(sys::fstat(guest_so.as_raw_fd()).unwrap().st_ino, 77);
    }
    let mut cfg = WlConfig::new(&host_sock);
    cfg.fences = backend_fences;
    let d = DaemonRun::with(
        &dir,
        Box::new(SyncobjConnector {
            cfg,
            caps: guest_caps,
            kernel: Arc::new(SyncobjKernel { guest }),
            host: vec![(77, host_so)],
        }),
    );

    let s = UnixStream::connect(&d.socket).unwrap();
    let mut w = s.try_clone().unwrap();
    w.write_all(
        &MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish(),
    )
    .unwrap();
    w.write_all(
        &MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(3)
            .finish(),
    )
    .unwrap();
    let (mut buf, mut fds) = (Vec::new(), Vec::new());
    let mut offered = None;
    read_msgs(&s, &mut buf, &mut fds, |obj, opc, m, _| {
        if obj == 2 && opc == op::wl_registry::EVT_GLOBAL {
            let len = word(m, 3) as usize;
            if &m[16..16 + len - 1] == b"wp_linux_drm_syncobj_manager_v1" {
                offered = Some(word(m, 2));
            }
        }
        obj == 3
    });
    let Some(name) = offered else {
        return (false, None, host_ino);
    };
    w.write_all(
        &MsgBuilder::new(2, op::wl_registry::REQ_BIND)
            .uint(name)
            .generic_new_id("wp_linux_drm_syncobj_manager_v1", 1, 4)
            .finish(),
    )
    .unwrap();
    let import = MsgBuilder::new(4, op::wp_linux_drm_syncobj_manager_v1::REQ_IMPORT_TIMELINE)
        .new_id(5)
        .finish();
    sys::send_with_fds(s.as_raw_fd(), &import, &[guest_so.as_raw_fd()]).unwrap();
    w.write_all(
        &MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(6)
            .finish(),
    )
    .unwrap();
    read_msgs(&s, &mut buf, &mut fds, |obj, _, _, _| obj == 6);
    let got = rx.recv_timeout(Duration::from_secs(5)).ok();
    (true, got, host_ino)
}

const SYNCOBJ_CAPS: u32 = uapi::CAP_WAYLAND | uapi::CAP_SYNCOBJ;

#[test]
fn a_client_timeline_reaches_the_compositor_as_the_host_syncobj_behind_it() {
    let (offered, got, host_ino) = syncobj_round_trip("y1", SYNCOBJ_CAPS, true, true);
    assert!(offered, "the syncobj global is offered");
    let got = got.expect("import_timeline reached the compositor");
    assert_eq!(
        sys::fstat(got.as_raw_fd()).unwrap().st_ino,
        host_ino,
        "what the compositor imports is the host syncobj, not the guest's file"
    );
}

#[test]
fn a_syncobj_the_guest_kernel_does_not_know_reaches_the_compositor_as_a_placeholder() {
    let (offered, got, host_ino) = syncobj_round_trip("y2", SYNCOBJ_CAPS, true, false);
    assert!(offered);
    let got = got.expect("the message still carries one descriptor");
    assert_ne!(sys::fstat(got.as_raw_fd()).unwrap().st_ino, host_ino);
    let link = std::fs::read_link(format!("/proc/self/fd/{}", got.as_raw_fd())).unwrap();
    assert!(
        !link.to_string_lossy().contains("guest-syncobj"),
        "{link:?}"
    );
}

#[test]
fn the_syncobj_global_needs_fences_on_both_ends() {
    // A guest kernel without fences, and a backend without them.
    assert!(!syncobj_round_trip("y3", uapi::CAP_WAYLAND, true, true).0);
    assert!(!syncobj_round_trip("y4", SYNCOBJ_CAPS, false, true).0);
}
