//! End to end, minus the VM: real clients → the guest daemon → an in-process
//! channel → the backend's `WlConn` → a real headless compositor.
//!
//! The channel here is the backend's own connection object called directly,
//! with the kernel's two jobs (resolving a guest dma-buf, adopting a DRM file)
//! stubbed -- there is no GPU -- so everything between a client's socket and
//! the compositor's is the code that ships: both engines, the frame format,
//! the reader thread, stream pumping, shm copying, blobs, the allowlist.
//!
//! Needs sway, wayland-info, wl-clipboard and weston's demo clients on PATH,
//! so it is ignored by default; `scripts/wl-loopback-test.sh` provides them
//! with nix and runs it.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use device::hostfd::HandleKind;
use device::wl::{HostFds, RecvOps, SendOps, WlConfig, WlConn};
use nvgpu_wl_guest::channel::{Channel, Connector, HostInfo, Received, Sent};
use nvgpu_wl_guest::daemon::{Config, Daemon, Totals};
use nvgpu_wl_guest::uapi;
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
        // The kernel's SEND: a dma-buf it cannot resolve goes as invalid.
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
    fn start(dir: &Path, host_socket: &Path) -> DaemonRun {
        let socket = dir.join("proxy-0");
        let cfg = Config::new(&socket);
        let wl = WlConfig::new(host_socket);
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut d = Daemon::new(cfg, Box::new(LoopConnector { cfg: wl })).unwrap();
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
    let dir = runtime_dir("s");
    let (_sway, host_sock) = start_sway(&dir);
    let _kbd = virtual_keyboard(&host_sock);
    let d = DaemonRun::start(&dir, &host_sock);
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
    let dir = runtime_dir("w");
    std::fs::create_dir_all(&dir).unwrap();
    let (_weston, host_sock) = start_weston(&dir);
    let d = DaemonRun::start(&dir, &host_sock);
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
        assert!(!proxied.contains(hidden), "{hidden} leaked through the proxy");
    }
    let mut shm = client("weston-presentation-shm", &["-f"], &proxy, &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    assert!(shm.try_wait().unwrap().is_none(), "weston-presentation-shm exited early");
    let _ = shm.kill();
    let _ = shm.wait();
    let t = settle(&d, |t| t.clients >= 2 && t.commits > 0);
    println!("weston: {t:?}");
    assert!(t.commits >= 20 && t.shm_syncs >= 20, "{t:?}");
    assert!(t.time_rewrites >= 10, "{t:?}");
    assert!(t.errors.is_empty(), "protocol errors: {:?}", t.errors);
}
