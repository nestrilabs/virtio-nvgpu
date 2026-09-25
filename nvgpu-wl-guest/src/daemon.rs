//! The guest daemon's event loop: one thread, one epoll set.
//!
//! Normal mode: guest applications connect to our socket
//! (`$XDG_RUNTIME_DIR/wayland-0`), and each connection gets a channel to the
//! host (one backend handle, one host compositor connection). Export mode:
//! the host's export socket hands us connections from host applications, and
//! each gets a connection to the guest compositor instead. Either way a
//! connection is a [`Client`]: a local socket, a channel, and a
//! `wlwire::engine::Engine` between them.
//!
//! Backpressure: the local socket is read only while the channel is taking
//! frames. When the host says it is busy (its compositor is not reading), the
//! frame is kept and retried, and the local peer waits in its own socket
//! buffer. The channel is read whenever it is readable, and what it yields is
//! written to the local peer as fast as the peer takes it.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wlwire::engine::{Blame, Engine, EngineConfig, Fatal, Local, Platform, Rewrites, Side};
use wlwire::frame::{self, Desc, DescOut, Unit};
use wlwire::policy::{LeaseGate, Policy};
use wlwire::sys;

use crate::channel::{Channel, Connector, HostInfo, Sent};
use crate::uapi;

#[derive(Clone, Debug)]
pub struct Config {
    /// Normal mode: the socket guest clients connect to.
    pub listen: PathBuf,
    /// Export mode: the guest compositor's socket; `listen` is unused.
    pub export_to: Option<PathBuf>,
    /// The guest card node DRM files (leases) are cloned from; found
    /// automatically when unset.
    pub card: Option<PathBuf>,
    /// The guest render node host dma-bufs are imported into (export mode).
    pub render: Option<PathBuf>,
    /// Refresh the clock offset this often.
    pub clock_refresh: Duration,
}

impl Config {
    pub fn new(listen: impl Into<PathBuf>) -> Self {
        Self {
            listen: listen.into(),
            export_to: None,
            card: None,
            render: None,
            clock_refresh: Duration::from_secs(5),
        }
    }
}

/// Counters over every connection the daemon has had.
#[derive(Clone, Debug, Default)]
pub struct Totals {
    pub clients: u64,
    pub msgs_to_host: u64,
    pub msgs_to_local: u64,
    pub commits: u64,
    pub shm_syncs: u64,
    pub shm_bytes: u64,
    pub blobs_received: u64,
    pub blobs_sent: u64,
    pub streams: u64,
    pub stream_bytes_in: u64,
    pub stream_bytes_out: u64,
    pub globals_offered: u64,
    pub time_rewrites: u64,
    pub devt_rewrites: u64,
    pub released_synthesised: u64,
    pub placeholders: u64,
    pub errors: Vec<String>,
}

impl Totals {
    fn add(&mut self, e: &Engine) {
        let s = &e.stats;
        self.msgs_to_host += s.msgs_to_channel;
        self.msgs_to_local += s.msgs_to_local;
        self.commits += s.commits;
        self.globals_offered += s.globals_offered;
        self.time_rewrites += s.time_rewrites;
        self.devt_rewrites += s.devt_rewrites;
        self.released_synthesised += s.released_synthesised;
        self.placeholders += s.placeholders;
        let (syncs, bytes) = e.shm_stats();
        self.shm_syncs += syncs;
        self.shm_bytes += bytes;
        let (sent, recv) = e.blob_stats();
        self.blobs_sent += sent;
        self.blobs_received += recv;
        let (n, out, inn) = e.stream_stats();
        self.streams += n;
        self.stream_bytes_out += out;
        self.stream_bytes_in += inn;
    }
}

const TOK_LISTENER: u64 = u64::MAX;
const TOK_EXPORT: u64 = u64::MAX - 1;
const SUB_SOCK: u64 = 0;
const SUB_CHAN: u64 = 1;
const SUB_STREAM: u64 = 1 << 31;

/// What one RECV asks the host for (at least `frame::MIN_FRAME`, at most
/// what HELLO allows).
const RECV_BYTES: usize = 256 * 1024;

/// The guest kernel as the engine's platform: dma-bufs and DRM files are its
/// to resolve, in SEND and RECV.
struct GuestPlat;

impl Platform for GuestPlat {
    fn dmabuf_out(&mut self, fd: OwnedFd) -> DescOut {
        DescOut {
            desc: Desc::new(frame::DESC_DMABUF),
            fd: Some(fd),
        }
    }
    fn dmabuf_in(&mut self, _d: &Desc, fd: Option<OwnedFd>) -> io::Result<OwnedFd> {
        fd.ok_or_else(|| io::Error::other("the kernel could not import the dma-buf"))
    }
    fn drm_file_out(&mut self, _fd: OwnedFd) -> DescOut {
        // A guest compositor's leases are not exported to the host.
        DescOut::plain(Desc::invalid(frame::DESC_DRM_FILE))
    }
    fn drm_file_in(&mut self, _d: &Desc, fd: Option<OwnedFd>) -> io::Result<OwnedFd> {
        fd.ok_or_else(|| io::Error::other("the kernel could not adopt the DRM file"))
    }
    fn syncobj_out(&mut self, fd: OwnedFd) -> DescOut {
        // The kernel names the host syncobj behind it, or marks it invalid.
        DescOut {
            desc: Desc::new(frame::DESC_SYNCOBJ),
            fd: Some(fd),
        }
    }
}

struct Client {
    sock: UnixStream,
    chan: Box<dyn Channel>,
    engine: Engine,
    inbuf: Vec<u8>,
    infds: VecDeque<OwnedFd>,
    /// Frames the host has not taken yet.
    tx: VecDeque<(Vec<u8>, Vec<Option<OwnedFd>>)>,
    streams: HashMap<RawFd, u32>,
    sock_events: u32,
    closing: bool,
}

pub struct Daemon {
    cfg: Config,
    conn: Box<dyn Connector>,
    info: HostInfo,
    clock: Arc<AtomicI64>,
    last_clock: Instant,
    ep: OwnedFd,
    listener: Option<UnixListener>,
    export: Option<Box<dyn Channel>>,
    clients: Vec<Option<Client>>,
    card: Option<OwnedFd>,
    render: Option<OwnedFd>,
    hello_caps: u32,
    stop: Arc<AtomicBool>,
    totals: Arc<Mutex<Totals>>,
}

fn epoll_ctl(ep: RawFd, op: i32, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: token };
    let r = unsafe { libc::epoll_ctl(ep, op, fd, &mut ev) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A DRM node of ours: `DRM_IOCTL_VERSION` says "nvidia-drm".
fn is_nvidia_drm(fd: RawFd) -> bool {
    #[repr(C)]
    struct DrmVersion {
        major: i32,
        minor: i32,
        patch: i32,
        name_len: usize,
        name: *mut u8,
        date_len: usize,
        date: *mut u8,
        desc_len: usize,
        desc: *mut u8,
    }
    let mut name = [0u8; 32];
    let mut v = DrmVersion {
        major: 0,
        minor: 0,
        patch: 0,
        name_len: name.len(),
        name: name.as_mut_ptr(),
        date_len: 0,
        date: std::ptr::null_mut(),
        desc_len: 0,
        desc: std::ptr::null_mut(),
    };
    // DRM_IOCTL_VERSION = _IOWR('d', 0x00, struct drm_version)
    let req = (3u64 << 30) | ((size_of::<DrmVersion>() as u64) << 16) | ((b'd' as u64) << 8);
    let r = unsafe { libc::ioctl(fd, req as _, &mut v) };
    r == 0 && name.starts_with(b"nvidia-drm")
}

/// Open the first of `prefix*` in /dev/dri that is ours (or `explicit`).
fn open_node(explicit: Option<&Path>, prefix: &str) -> Option<OwnedFd> {
    let candidates: Vec<PathBuf> = match explicit {
        Some(p) => vec![p.to_path_buf()],
        None => {
            let mut v: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
                .ok()?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
                })
                .collect();
            v.sort();
            v
        }
    };
    for p in candidates {
        let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else {
            continue;
        };
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            continue;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if is_nvidia_drm(fd.as_raw_fd()) {
            if prefix == "card" {
                // The first opener of a card node becomes the guest core's
                // master; a template must never keep that from a compositor.
                // DRM_IOCTL_DROP_MASTER = _IO('d', 0x1f).
                unsafe { libc::ioctl(fd.as_raw_fd(), ((b'd' as u64) << 8 | 0x1f) as _) };
            }
            return Some(fd);
        }
    }
    None
}

impl Daemon {
    pub fn new(cfg: Config, mut conn: Box<dyn Connector>) -> io::Result<Daemon> {
        let info = conn.info()?;
        let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if ep < 0 {
            return Err(io::Error::last_os_error());
        }
        let ep = unsafe { OwnedFd::from_raw_fd(ep) };
        let mut d = Daemon {
            clock: Arc::new(AtomicI64::new(info.clock_offset_ns)),
            last_clock: Instant::now(),
            info,
            cfg,
            conn,
            ep,
            listener: None,
            export: None,
            clients: Vec::new(),
            card: None,
            render: None,
            hello_caps: 0,
            stop: Arc::new(AtomicBool::new(false)),
            totals: Arc::new(Mutex::new(Totals::default())),
        };
        if d.info.max_frame < frame::MIN_FRAME {
            return Err(io::Error::other(format!(
                "the kernel's frame limit {} is too small",
                d.info.max_frame
            )));
        }
        if d.cfg.export_to.is_some() {
            if d.info.caps & uapi::CAP_EXPORT == 0 {
                return Err(io::Error::other(
                    "the host exports no socket (--wayland-export)",
                ));
            }
            if d.info.caps & uapi::CAP_DMABUF_IMPORT != 0 {
                d.render = open_node(d.cfg.render.as_deref(), "renderD");
                if d.render.is_some() {
                    d.hello_caps |= frame::HELLO_G_DMABUF_IMPORT;
                }
            }
            let ch = d.conn.connect(uapi::LISTEN)?;
            epoll_ctl(
                d.ep.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                ch.poll_fd(),
                libc::EPOLLIN as u32,
                TOK_EXPORT,
            )?;
            d.export = Some(ch);
        } else {
            if d.info.caps & uapi::CAP_WAYLAND == 0 {
                return Err(io::Error::other(
                    "the host has no Wayland socket configured",
                ));
            }
            if d.info.caps & uapi::CAP_DRM_FILE != 0 {
                d.card = open_node(d.cfg.card.as_deref(), "card");
                if d.card.is_some() {
                    d.hello_caps |= frame::HELLO_G_DRM_FILE;
                }
            }
            // Explicit sync: the kernel can name a client's syncobj by its
            // host syncobj (the backend serves fences).
            if d.info.caps & uapi::CAP_SYNCOBJ != 0 {
                d.hello_caps |= frame::HELLO_G_SYNCOBJ;
            }
            let _ = std::fs::remove_file(&d.cfg.listen);
            let l = UnixListener::bind(&d.cfg.listen)?;
            l.set_nonblocking(true)?;
            epoll_ctl(
                d.ep.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                l.as_raw_fd(),
                libc::EPOLLIN as u32,
                TOK_LISTENER,
            )?;
            d.listener = Some(l);
        }
        Ok(d)
    }

    /// Set to stop `run`.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    pub fn totals(&self) -> Arc<Mutex<Totals>> {
        self.totals.clone()
    }

    /// A snapshot including live connections.
    pub fn snapshot(&self) -> Totals {
        let mut t = self.totals.lock().unwrap().clone();
        for c in self.clients.iter().flatten() {
            t.add(&c.engine);
        }
        t
    }

    pub fn run(&mut self) -> io::Result<()> {
        while !self.stop.load(Ordering::Relaxed) {
            self.turn(100)?;
        }
        for i in 0..self.clients.len() {
            self.close(i);
        }
        Ok(())
    }

    /// One wait and everything it woke.
    pub fn turn(&mut self, max_wait_ms: i32) -> io::Result<()> {
        let busy = self.clients.iter().flatten().any(|c| !c.tx.is_empty());
        let timeout = if busy {
            5.min(max_wait_ms)
        } else {
            max_wait_ms
        };
        let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 64];
        let n = unsafe {
            libc::epoll_wait(
                self.ep.as_raw_fd(),
                evs.as_mut_ptr(),
                evs.len() as i32,
                timeout,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(e)
            };
        }
        let mut touched = Vec::new();
        for ev in &evs[..n as usize] {
            let (tok, events) = (ev.u64, ev.events);
            match tok {
                TOK_LISTENER => self.accept(),
                TOK_EXPORT => self.accept_export(),
                _ => {
                    let slot = (tok >> 32) as usize;
                    let sub = tok & 0xffff_ffff;
                    if self.clients.get(slot).is_none_or(|c| c.is_none()) {
                        continue;
                    }
                    match sub {
                        SUB_SOCK => {
                            let gone = events & (libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0;
                            let c = self.clients[slot].as_mut().unwrap();
                            if gone && !c.tx.is_empty() {
                                // The client is gone with frames still waiting
                                // for a busy host. read_local will not read
                                // while they wait, so the EOF that closes the
                                // slot would never be seen, and HUP cannot be
                                // masked: every epoll_wait would return at once
                                // and retry the SEND, at 100% CPU and a
                                // virtqueue round trip each, until the host's
                                // compositor drained. Nobody is left to read
                                // the replies to those frames, and closing the
                                // channel ends the host's side of the client
                                // anyway.
                                c.tx.clear();
                                c.closing = true;
                            } else if events
                                & (libc::EPOLLIN | libc::EPOLLHUP | libc::EPOLLERR) as u32
                                != 0
                            {
                                self.read_local(slot);
                            }
                            if events & libc::EPOLLOUT as u32 != 0 {
                                self.write_local(slot);
                            }
                        }
                        SUB_CHAN => self.read_channel(slot),
                        s if s & SUB_STREAM != 0 => {
                            let fd = (s & !SUB_STREAM) as RawFd;
                            let c = self.clients[slot].as_mut().unwrap();
                            if let Some(&id) = c.streams.get(&fd) {
                                let rd = events
                                    & (libc::EPOLLIN | libc::EPOLLHUP | libc::EPOLLERR) as u32
                                    != 0;
                                let wr = events & (libc::EPOLLOUT | libc::EPOLLERR) as u32 != 0;
                                c.engine.stream_io(id, rd, wr);
                            }
                            self.pump_tx(slot);
                        }
                        _ => {}
                    }
                    touched.push(slot);
                }
            }
        }
        // Retry frames the host refused, and keep every registration current.
        for slot in 0..self.clients.len() {
            if self.clients[slot]
                .as_ref()
                .is_some_and(|c| !c.tx.is_empty())
            {
                self.pump_tx(slot);
                touched.push(slot);
            }
        }
        touched.sort_unstable();
        touched.dedup();
        for slot in touched {
            self.sync(slot);
        }
        if self.last_clock.elapsed() >= self.cfg.clock_refresh {
            self.last_clock = Instant::now();
            if let Ok(i) = self.conn.info() {
                self.clock.store(i.clock_offset_ns, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn engine_config(&self, local: Local) -> EngineConfig {
        EngineConfig {
            side: Side::Guest,
            local,
            policy: Policy {
                drm_file: self.hello_caps & frame::HELLO_G_DRM_FILE != 0,
                // The host decides which lease devices are ours.
                lease: LeaseGate::Allow,
                fences: self.hello_caps & frame::HELLO_G_SYNCOBJ != 0,
            },
            rewrites: Some(Rewrites {
                devmap: self.info.devmap.clone(),
                clock_offset: self.clock.clone(),
            }),
            synth_released: local == Local::Client,
        }
    }

    fn add_client(&mut self, sock: UnixStream, chan: Box<dyn Channel>, local: Local) {
        if sock.set_nonblocking(true).is_err() {
            return;
        }
        let mut engine = Engine::new(self.engine_config(local));
        engine.hello(self.hello_caps);
        let slot = match self.clients.iter().position(|c| c.is_none()) {
            Some(s) => s,
            None => {
                self.clients.push(None);
                self.clients.len() - 1
            }
        };
        let base = (slot as u64) << 32;
        let ep = self.ep.as_raw_fd();
        let ev = (libc::EPOLLIN | libc::EPOLLRDHUP) as u32;
        if epoll_ctl(
            ep,
            libc::EPOLL_CTL_ADD,
            sock.as_raw_fd(),
            ev,
            base | SUB_SOCK,
        )
        .is_err()
            || epoll_ctl(
                ep,
                libc::EPOLL_CTL_ADD,
                chan.poll_fd(),
                libc::EPOLLIN as u32,
                base | SUB_CHAN,
            )
            .is_err()
        {
            return;
        }
        self.clients[slot] = Some(Client {
            sock,
            chan,
            engine,
            inbuf: Vec::new(),
            infds: VecDeque::new(),
            tx: VecDeque::new(),
            streams: HashMap::new(),
            sock_events: ev,
            closing: false,
        });
        self.totals.lock().unwrap().clients += 1;
        self.pump_tx(slot);
        // The channel may already be readable (the host's HELLO).
        self.read_channel(slot);
        self.sync(slot);
    }

    fn accept(&mut self) {
        loop {
            let Some(l) = &self.listener else { return };
            match l.accept() {
                Ok((s, _)) => match self.conn.connect(uapi::CONNECT) {
                    Ok(ch) => self.add_client(s, ch, Local::Client),
                    Err(e) => {
                        eprintln!("nvgpu-wl-guest: cannot open a channel to the host: {e}");
                        let err = Fatal {
                            object: 1,
                            code: wlwire::engine::ERR_IMPLEMENTATION,
                            message: format!("virtio-nvgpu: no host channel: {e}"),
                            blame: Blame::Remote,
                        };
                        let _ = sys::send_with_fds(s.as_raw_fd(), &err.display_error(), &[]);
                    }
                },
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    eprintln!("nvgpu-wl-guest: accept: {e}");
                    return;
                }
            }
        }
    }

    fn accept_export(&mut self) {
        let Some(target) = self.cfg.export_to.clone() else {
            return;
        };
        for _ in 0..16 {
            let ch = match self.conn.connect(uapi::ACCEPT) {
                Ok(c) => c,
                Err(_) => return,
            };
            match UnixStream::connect(&target) {
                Ok(s) => self.add_client(s, ch, Local::Server),
                Err(e) => eprintln!(
                    "nvgpu-wl-guest: cannot reach the guest compositor at {}: {e}",
                    target.display()
                ),
            }
        }
    }

    fn fatal(&mut self, slot: usize, f: Fatal) {
        let Some(c) = self.clients[slot].as_mut() else {
            return;
        };
        self.totals
            .lock()
            .unwrap()
            .errors
            .push(format!("{:?}: {}", f.blame, f.message));
        eprintln!(
            "nvgpu-wl-guest: closing a connection: {:?}: {}",
            f.blame, f.message
        );
        match (c.engine.local_is_client(), f.blame) {
            // Our client gets the error it earned, or the host's verdict.
            (true, _) => {
                let _ = c.engine.local_out().flush(c.sock.as_raw_fd());
                let _ = sys::send_with_fds(c.sock.as_raw_fd(), &f.display_error(), &[]);
            }
            // The guest compositor broke the protocol: tell the host client.
            (false, Blame::Local) => {
                let mut q = VecDeque::from([f.record()]);
                let (mut fr, fds) =
                    frame::pack(&mut q, self.info.max_frame, frame::MAX_DESC, false);
                let _ = c.chan.send(&mut fr, &fds);
            }
            (false, _) => {}
        }
        c.closing = true;
    }

    fn read_local(&mut self, slot: usize) {
        let mut buf = vec![0u8; 64 * 1024];
        for _ in 0..16 {
            let c = self.clients[slot].as_mut().unwrap();
            if c.closing || !c.tx.is_empty() {
                break;
            }
            let mut fds = Vec::new();
            match sys::recv_with_fds(c.sock.as_raw_fd(), &mut buf, &mut fds) {
                Ok(0) => {
                    c.closing = true;
                    break;
                }
                Ok(n) => {
                    c.inbuf.extend_from_slice(&buf[..n]);
                    c.infds.extend(fds);
                    let Client {
                        engine,
                        inbuf,
                        infds,
                        ..
                    } = c;
                    if let Err(f) = engine.from_local(inbuf, infds, &mut GuestPlat) {
                        self.fatal(slot, f);
                        break;
                    }
                    self.pump_tx(slot);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    c.closing = true;
                    break;
                }
            }
        }
    }

    fn write_local(&mut self, slot: usize) {
        let c = self.clients[slot].as_mut().unwrap();
        if let Err(e) = c.engine.local_out().flush(c.sock.as_raw_fd()) {
            eprintln!("nvgpu-wl-guest: writing to a client: {e}");
            c.closing = true;
        }
    }

    /// Frame whatever the engine has for the host and send it, in order.
    fn pump_tx(&mut self, slot: usize) {
        let max = self.info.max_frame;
        let Some(c) = self.clients[slot].as_mut() else {
            return;
        };
        let mut units: VecDeque<Unit> = c.engine.take_units();
        while !units.is_empty() {
            let (f, fds) = frame::pack(&mut units, max, frame::MAX_DESC, false);
            c.tx.push_back((f, fds));
        }
        while let Some((f, fds)) = c.tx.front_mut() {
            match c.chan.send(f, fds) {
                Ok(Sent::Accepted { .. }) => {
                    c.tx.pop_front();
                }
                Ok(Sent::Busy) => break,
                Err(e) => {
                    // The host ended the connection; its reason, if any, is
                    // waiting in the channel.
                    eprintln!("nvgpu-wl-guest: the host refused a frame: {e}");
                    c.tx.clear();
                    self.read_channel(slot);
                    if let Some(c) = self.clients[slot].as_mut() {
                        c.closing = true;
                    }
                    return;
                }
            }
        }
    }

    fn read_channel(&mut self, slot: usize) {
        // Not the whole frame limit (4 MiB with indirect descriptors): the
        // kernel sizes its response buffer, and the backend what it packs,
        // by what is asked, and what arrives per present is a few small
        // events. The backend never splits a record and says F_MORE when
        // more waits, which keeps the channel readable, so a burst only
        // takes more RECVs.
        let max = self.info.max_frame.min(RECV_BYTES.max(frame::MIN_FRAME));
        let card = self.card.as_ref().map(|f| f.as_raw_fd());
        let render = self.render.as_ref().map(|f| f.as_raw_fd());
        for _ in 0..64 {
            let Some(c) = self.clients[slot].as_mut() else {
                return;
            };
            let r = match c.chan.recv(max, card, render) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("nvgpu-wl-guest: channel: {e}");
                    c.closing = true;
                    return;
                }
            };
            let empty = frame::decode(&r.frame)
                .map(|f| f.records.is_empty())
                .unwrap_or(true);
            if !empty {
                if let Err(f) = c.engine.from_channel(&r.frame, r.fds, &mut GuestPlat) {
                    self.fatal(slot, f);
                    return;
                }
                let c = self.clients[slot].as_mut().unwrap();
                if let Err(e) = c.engine.local_out().flush(c.sock.as_raw_fd()) {
                    eprintln!("nvgpu-wl-guest: writing to a client: {e}");
                    c.closing = true;
                }
                self.pump_tx(slot);
            }
            let Some(c) = self.clients[slot].as_mut() else {
                return;
            };
            if c.engine.hung_up() {
                c.closing = true;
                return;
            }
            if !r.more {
                return;
            }
        }
    }

    /// Bring epoll up to date with what the client wants, or close it.
    fn sync(&mut self, slot: usize) {
        let ep = self.ep.as_raw_fd();
        let Some(c) = self.clients[slot].as_mut() else {
            return;
        };
        if c.closing {
            let _ = c.engine.local_out().flush(c.sock.as_raw_fd());
            self.close(slot);
            return;
        }
        let base = (slot as u64) << 32;
        // Nothing of the client's is read while frames wait for the host, so
        // nothing that says "readable" is asked for then either: not EPOLLIN,
        // and not EPOLLRDHUP, which a client that shut down only its writing
        // side would report on every wait. A client that is gone altogether
        // still wakes us with EPOLLHUP, which cannot be masked (turn).
        let mut want = 0;
        if c.tx.is_empty() {
            want |= (libc::EPOLLIN | libc::EPOLLRDHUP) as u32;
        }
        if c.engine.local_out_len() > 0 {
            want |= libc::EPOLLOUT as u32;
        }
        if want != c.sock_events {
            let _ = epoll_ctl(
                ep,
                libc::EPOLL_CTL_MOD,
                c.sock.as_raw_fd(),
                want,
                base | SUB_SOCK,
            );
            c.sock_events = want;
        }
        // Streams are registered one-shot and re-armed here: the engine closes
        // a stream's descriptor itself, and a sink's descriptor shares its
        // open file with the client that sent it, so epoll would otherwise
        // keep reporting a registration nobody can remove any more.
        let interest = c.engine.stream_interest();
        c.streams = interest.iter().map(|i| (i.fd, i.id)).collect();
        for i in interest {
            let mut ev = libc::EPOLLONESHOT as u32;
            if i.read {
                ev |= libc::EPOLLIN as u32;
            }
            if i.write {
                ev |= libc::EPOLLOUT as u32;
            }
            let tok = base | SUB_STREAM | i.fd as u64;
            if epoll_ctl(ep, libc::EPOLL_CTL_MOD, i.fd, ev, tok).is_err() {
                let _ = epoll_ctl(ep, libc::EPOLL_CTL_ADD, i.fd, ev, tok);
            }
        }
    }

    fn close(&mut self, slot: usize) {
        if let Some(c) = self.clients[slot].take() {
            self.totals.lock().unwrap().add(&c.engine);
            // Dropping the socket, the channel (CLOSE on the host handle) and
            // the engine's streams removes them from epoll.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Received;
    use std::sync::atomic::AtomicUsize;

    /// A host whose compositor is not reading: every SEND is refused as
    /// busy, and there is never anything to receive.
    struct BusyChannel {
        sends: Arc<AtomicUsize>,
        /// The largest receive the daemon asked for.
        asked: Arc<AtomicUsize>,
        ready: OwnedFd,
    }

    impl Channel for BusyChannel {
        fn send(&mut self, _f: &mut [u8], _fds: &[Option<OwnedFd>]) -> io::Result<Sent> {
            self.sends.fetch_add(1, Ordering::Relaxed);
            Ok(Sent::Busy)
        }
        fn recv(
            &mut self,
            max: usize,
            _c: Option<RawFd>,
            _r: Option<RawFd>,
        ) -> io::Result<Received> {
            self.asked.fetch_max(max, Ordering::Relaxed);
            let (frame, _) = frame::pack(&mut VecDeque::new(), frame::MIN_FRAME, 0, false);
            Ok(Received {
                frame,
                fds: Vec::new(),
                more: false,
            })
        }
        fn poll_fd(&self) -> RawFd {
            self.ready.as_raw_fd()
        }
    }

    #[derive(Default)]
    struct BusyHost {
        sends: Arc<AtomicUsize>,
        asked: Arc<AtomicUsize>,
        max_frame: usize,
    }

    impl Connector for BusyHost {
        fn info(&mut self) -> io::Result<HostInfo> {
            Ok(HostInfo {
                caps: uapi::CAP_WAYLAND,
                clock_offset_ns: 0,
                max_frame: self.max_frame,
                devmap: Vec::new(),
            })
        }
        fn connect(&mut self, _mode: u32) -> io::Result<Box<dyn Channel>> {
            Ok(Box::new(BusyChannel {
                sends: self.sends.clone(),
                asked: self.asked.clone(),
                ready: sys::eventfd()?,
            }))
        }
    }

    fn socket_in_tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nvwl-daemon-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("wayland-0")
    }

    #[test]
    fn a_receive_asks_for_a_modest_frame_not_the_whole_limit() {
        let sock = socket_in_tmp("recv");
        let asked = Arc::new(AtomicUsize::new(0));
        let mut d = Daemon::new(
            Config::new(&sock),
            Box::new(BusyHost {
                asked: asked.clone(),
                max_frame: 4 << 20,
                ..Default::default()
            }),
        )
        .unwrap();
        let _client = UnixStream::connect(&sock).unwrap();
        d.turn(100).unwrap();
        // The first receive is made as the client is added.
        assert_eq!(asked.load(Ordering::Relaxed), RECV_BYTES);
        assert!(RECV_BYTES >= frame::MIN_FRAME);
    }

    #[test]
    fn a_client_that_hangs_up_while_the_host_is_busy_is_closed_not_spun_on() {
        let sock = socket_in_tmp("hup");
        let sends = Arc::new(AtomicUsize::new(0));
        let mut d = Daemon::new(
            Config::new(&sock),
            Box::new(BusyHost {
                sends: sends.clone(),
                max_frame: 256 * 1024,
                ..Default::default()
            }),
        )
        .unwrap();
        let client = UnixStream::connect(&sock).unwrap();
        d.turn(100).unwrap();
        // The daemon's HELLO waits for the host.
        assert!(d.clients.iter().flatten().any(|c| !c.tx.is_empty()));
        drop(client);
        let before = sends.load(Ordering::Relaxed);
        for _ in 0..50 {
            d.turn(100).unwrap();
            if d.clients.iter().all(|c| c.is_none()) {
                break;
            }
        }
        assert!(
            d.clients.iter().all(|c| c.is_none()),
            "the slot of a client that hung up is closed"
        );
        let retried = sends.load(Ordering::Relaxed) - before;
        assert!(
            retried <= 2,
            "{retried} SENDs retried for a client that is gone"
        );
    }
}
