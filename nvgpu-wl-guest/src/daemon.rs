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
//!
//! What the daemon holds for the channel is one frame's worth per client, not
//! what the client's input comes to: the engine reads commits' copies and
//! blobs only as frames are made ([`Engine::take_units_upto`]), and stops
//! taking the client's input once [`wlwire::engine::CHANNEL_HIGH_WATER`]
//! bytes wait for the channel. What it has read and not taken stays in the
//! client's input buffer, and is taken as the channel drains ([`Daemon::drive`]).

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
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
    /// The most bytes one client has been seen not to have read (bounded
    /// by `LOCAL_OUT_MAX` and one received frame).
    pub peak_unread: u64,
    /// The first [`MAX_ERRORS`] connections' fatal errors.
    pub errors: Vec<String>,
    /// Every connection's.
    pub error_count: u64,
}

/// Fatal errors kept in [`Totals::errors`]: every client can cause one, and
/// the daemon runs as long as the guest does.
pub const MAX_ERRORS: usize = 32;

impl Totals {
    fn error(&mut self, e: String) {
        self.error_count += 1;
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(e);
        }
    }

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
const _: () = assert!(RECV_BYTES >= frame::MIN_FRAME);

/// Bytes the daemon holds for one client that has not read them. Past this
/// the client's channel is not read any more until it catches up: its
/// output waits in the backend, charged to that client's share of the VM's
/// queue budget, instead of here, where a client flooding requests whose
/// replies it never reads grew the daemon until the guest's OOM killer took
/// it and every client with it (W2). A native compositor drops only the
/// stuck client, and so does this: after [`STUCK_FOR`] over the line.
const LOCAL_OUT_MAX: usize = 4 << 20;
const STUCK_FOR: Duration = Duration::from_secs(30);

/// Lines a client can cause (its protocol errors, its socket's): at most
/// [`LOG_BURST`] each [`LOG_WINDOW`], a line the same as the one before is
/// counted rather than repeated, and what is dropped is said once the window
/// ends. Every guest process can connect, and the daemon's stderr is the
/// guest journal.
struct LogLimit {
    window: Instant,
    lines: u32,
    dropped: u64,
    last: String,
    same: u64,
}

const LOG_BURST: u32 = 20;
const LOG_WINDOW: Duration = Duration::from_secs(10);

impl LogLimit {
    fn new() -> Self {
        Self {
            window: Instant::now(),
            lines: 0,
            dropped: 0,
            last: String::new(),
            same: 0,
        }
    }

    /// What to print for `line` at `now`.
    fn lines(&mut self, line: String, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        if now.saturating_duration_since(self.window) >= LOG_WINDOW {
            if self.dropped > 0 {
                out.push(format!(
                    "nvgpu-wl-guest: ({} lines dropped by the log limit)",
                    self.dropped
                ));
            }
            self.window = now;
            self.lines = 0;
            self.dropped = 0;
        }
        if line == self.last {
            self.same += 1;
            return out;
        }
        if self.same > 0 {
            out.push(format!(
                "nvgpu-wl-guest: (the line before, {} more times)",
                self.same
            ));
            self.same = 0;
        }
        self.last.clone_from(&line);
        if self.lines >= LOG_BURST {
            self.dropped += 1;
            return out;
        }
        self.lines += 1;
        out.push(line);
        out
    }

    fn say(&mut self, line: String) {
        for l in self.lines(line, Instant::now()) {
            eprintln!("{l}");
        }
    }
}

/// The process at the other end of a client socket (SO_PEERCRED), in the
/// daemon's PID namespace; `None` when the kernel cannot say.
fn peer_pid(s: &UnixStream) -> Option<i32> {
    crate::sys::peer_cred(s.as_raw_fd())
        .map(|c| c.pid)
        .filter(|&pid| pid > 0)
}

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
    /// What the channel is watched for: nothing while the client has
    /// [`LOCAL_OUT_MAX`] unread.
    chan_events: u32,
    /// Since when the client has had that much unread.
    stuck_since: Option<Instant>,
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
    log: LogLimit,
}

fn epoll_ctl(ep: RawFd, op: i32, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    crate::sys::epoll_ctl(ep, op, fd, events, token)
}

/// A DRM node of ours: `DRM_IOCTL_VERSION` says "nvidia-drm".
fn is_nvidia_drm(fd: RawFd) -> bool {
    crate::sys::drm_driver_name(fd).is_some_and(|n| n.starts_with(b"nvidia-drm"))
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
        // Read-write, close-on-exec (std's default).
        let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(&p) else {
            continue;
        };
        let fd = OwnedFd::from(f);
        if is_nvidia_drm(fd.as_raw_fd()) {
            if prefix == "card" {
                // The first opener of a card node becomes the guest core's
                // master; a template must never keep that from a compositor.
                crate::sys::drop_master(fd.as_raw_fd());
            }
            return Some(fd);
        }
    }
    None
}

impl Daemon {
    pub fn new(cfg: Config, mut conn: Box<dyn Connector>) -> io::Result<Daemon> {
        let info = conn.info()?;
        let ep = crate::sys::epoll_create()?;
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
            log: LogLimit::new(),
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
        let n = match crate::sys::epoll_wait(self.ep.as_raw_fd(), &mut evs, timeout) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut touched = Vec::new();
        for ev in &evs[..n] {
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
                            self.drive(slot);
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
                self.drive(slot);
                touched.push(slot);
            }
            // A client that has fallen behind is looked at every turn, so
            // one that stays behind is closed even if nothing else happens.
            if self.clients[slot]
                .as_ref()
                .is_some_and(|c| c.stuck_since.is_some())
            {
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
        // A guest compositor's output is paced by the channel too (export
        // mode): its peer is the host client, behind the channel.
        engine.set_input_limit(Some(wlwire::engine::CHANNEL_HIGH_WATER));
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
            chan_events: libc::EPOLLIN as u32,
            stuck_since: None,
            closing: false,
        });
        self.totals.lock().unwrap().clients += 1;
        self.drive(slot);
        // The channel may already be readable (the host's HELLO).
        self.read_channel(slot);
        self.sync(slot);
    }

    fn accept(&mut self) {
        loop {
            let Some(l) = &self.listener else { return };
            match l.accept() {
                // Charged to the client, not to the daemon: each guest
                // process holds only a share of the VM's channels and their
                // budgets (NVGPU_WL_IOC_CONNECT_FOR).
                Ok((s, _)) => match match peer_pid(&s) {
                    Some(pid) => self.conn.connect_for(pid),
                    None => self.conn.connect(uapi::CONNECT),
                } {
                    Ok(ch) => self.add_client(s, ch, Local::Client),
                    Err(e) => {
                        self.log.say(format!(
                            "nvgpu-wl-guest: cannot open a channel to the host: {e}"
                        ));
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
                Err(e) => self.log.say(format!(
                    "nvgpu-wl-guest: cannot reach the guest compositor at {}: {e}",
                    target.display()
                )),
            }
        }
    }

    fn fatal(&mut self, slot: usize, f: Fatal) {
        let Some(c) = self.clients[slot].as_mut() else {
            return;
        };
        // Peer text: printable already (the engine's), quoted here.
        let text = wlwire::engine::printable(&f.message, 256);
        self.totals
            .lock()
            .unwrap()
            .error(format!("{:?}: {text}", f.blame));
        self.log.say(format!(
            "nvgpu-wl-guest: closing a connection: {:?}: {text:?}",
            f.blame
        ));
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
            if c.closing || !c.tx.is_empty() || c.engine.input_blocked() {
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
                    if c.infds.len() > wlwire::wire::MAX_FDS_QUEUED {
                        // As libwayland does with a client that overflows
                        // its descriptor ring.
                        let f = Fatal {
                            object: 1,
                            code: wlwire::engine::ERR_NO_MEMORY,
                            message: format!(
                                "more than {} file descriptors sent that no request takes",
                                wlwire::wire::MAX_FDS_QUEUED
                            ),
                            blame: Blame::Local,
                        };
                        self.fatal(slot, f);
                        break;
                    }
                    self.drive(slot);
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

    /// Send what the client's engine has for the host, and take more of the
    /// client's input as the channel takes it, until the host is busy, the
    /// engine's input limit is reached, or the input is used up.
    fn drive(&mut self, slot: usize) {
        loop {
            self.pump_tx(slot);
            let Some(c) = self.clients[slot].as_mut() else {
                return;
            };
            if c.closing || !c.tx.is_empty() || c.inbuf.is_empty() {
                return;
            }
            let before = c.inbuf.len();
            let Client {
                engine,
                inbuf,
                infds,
                ..
            } = c;
            if let Err(f) = engine.from_local(inbuf, infds, &mut GuestPlat) {
                self.fatal(slot, f);
                return;
            }
            let c = self.clients[slot].as_mut().unwrap();
            // Only part of a message is left, or nothing was taken.
            if c.inbuf.len() == before && !c.engine.has_channel_output() {
                return;
            }
        }
    }

    fn write_local(&mut self, slot: usize) {
        let c = self.clients[slot].as_mut().unwrap();
        if let Err(e) = c.engine.local_out().flush(c.sock.as_raw_fd()) {
            self.log
                .say(format!("nvgpu-wl-guest: writing to a client: {e}"));
            c.closing = true;
        }
    }

    /// Frame what the engine has for the host and send it, in order, a
    /// frame's worth at a time: until the host is busy (the frame is kept,
    /// and retried) or nothing is left.
    fn pump_tx(&mut self, slot: usize) {
        let max = self.info.max_frame;
        let Some(c) = self.clients[slot].as_mut() else {
            return;
        };
        loop {
            if c.tx.is_empty() {
                let mut units: VecDeque<Unit> = c.engine.take_units_upto(max);
                if units.is_empty() {
                    return;
                }
                while !units.is_empty() {
                    let (f, fds) = frame::pack(&mut units, max, frame::MAX_DESC, false);
                    c.tx.push_back((f, fds));
                }
            }
            let Some((f, fds)) = c.tx.front_mut() else {
                return;
            };
            match c.chan.send(f, fds) {
                Ok(Sent::Accepted { .. }) => {
                    c.tx.pop_front();
                }
                Ok(Sent::Busy) => return,
                Err(e) => {
                    // The host ended the connection; its reason, if any, is
                    // waiting in the channel.
                    self.log
                        .say(format!("nvgpu-wl-guest: the host refused a frame: {e}"));
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
            // A client that is not reading gets nothing more from the host
            // until it does (LOCAL_OUT_MAX); sync stops watching the channel.
            if c.engine.local_out_len() >= LOCAL_OUT_MAX {
                return;
            }
            let r = match c.chan.recv(max, card, render) {
                Ok(r) => r,
                Err(e) => {
                    self.log.say(format!("nvgpu-wl-guest: channel: {e}"));
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
                    self.log
                        .say(format!("nvgpu-wl-guest: writing to a client: {e}"));
                    c.closing = true;
                }
                let unread = c.engine.local_out_len() as u64;
                {
                    let mut t = self.totals.lock().unwrap();
                    t.peak_unread = t.peak_unread.max(unread);
                }
                self.drive(slot);
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
        // The channel is read only while the client keeps up (W2), and a
        // client that stays that far behind is dropped, as a compositor
        // drops a client it cannot write to.
        let behind = c.engine.local_out_len() >= LOCAL_OUT_MAX;
        let chan_want = if behind { 0 } else { libc::EPOLLIN as u32 };
        if behind {
            let since = *c.stuck_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= STUCK_FOR {
                self.log.say(format!(
                    "nvgpu-wl-guest: a client has not read {} bytes in {}s; closing it",
                    c.engine.local_out_len(),
                    STUCK_FOR.as_secs()
                ));
                self.close(slot);
                return;
            }
        } else {
            c.stuck_since = None;
        }
        if chan_want != c.chan_events {
            let _ = epoll_ctl(
                ep,
                libc::EPOLL_CTL_MOD,
                c.chan.poll_fd(),
                chan_want,
                base | SUB_CHAN,
            );
            c.chan_events = chan_want;
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

    /// A host scripted by the test: frames are taken or refused as busy as
    /// the test says, and what it pushes is received.
    #[derive(Clone)]
    struct Script {
        busy: Arc<AtomicBool>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        inbox: Arc<Mutex<VecDeque<Vec<u8>>>>,
        ready: Arc<OwnedFd>,
    }

    impl Script {
        fn new() -> Self {
            Self {
                busy: Arc::new(AtomicBool::new(false)),
                sent: Arc::new(Mutex::new(Vec::new())),
                inbox: Arc::new(Mutex::new(VecDeque::new())),
                ready: Arc::new(sys::eventfd().unwrap()),
            }
        }

        /// The host says `units`.
        fn push(&self, units: Vec<Unit>) {
            let mut q: VecDeque<Unit> = units.into();
            let (f, _) = frame::pack(&mut q, 1 << 20, 256, false);
            self.inbox.lock().unwrap().push_back(f);
            sys::eventfd_signal(self.ready.as_raw_fd());
        }

        /// Every record the daemon's frames carried, as (type, id, payload).
        fn records(&self) -> Vec<(u16, u32, Vec<u8>)> {
            let sent = self.sent.lock().unwrap();
            let mut v = Vec::new();
            for f in sent.iter() {
                let f = frame::decode(f).unwrap();
                for r in f.records() {
                    v.push((r.ty, r.id, r.payload.to_vec()));
                }
            }
            v
        }
    }

    struct ScriptChannel(Script);

    impl Channel for ScriptChannel {
        fn send(&mut self, f: &mut [u8], _fds: &[Option<OwnedFd>]) -> io::Result<Sent> {
            if self.0.busy.load(Ordering::Relaxed) {
                return Ok(Sent::Busy);
            }
            self.0.sent.lock().unwrap().push(f.to_vec());
            Ok(Sent::Accepted { backlog: 0 })
        }
        fn recv(
            &mut self,
            _max: usize,
            _c: Option<RawFd>,
            _r: Option<RawFd>,
        ) -> io::Result<Received> {
            let mut inbox = self.0.inbox.lock().unwrap();
            let frame = match inbox.pop_front() {
                Some(f) => f,
                None => frame::pack(&mut VecDeque::new(), frame::MIN_FRAME, 0, false).0,
            };
            if inbox.is_empty() {
                sys::eventfd_clear(self.0.ready.as_raw_fd());
            }
            Ok(Received {
                frame,
                fds: Vec::new(),
                more: !inbox.is_empty(),
            })
        }
        fn poll_fd(&self) -> RawFd {
            self.0.ready.as_raw_fd()
        }
    }

    struct ScriptHost(Script);

    impl Connector for ScriptHost {
        fn info(&mut self) -> io::Result<HostInfo> {
            Ok(HostInfo {
                caps: uapi::CAP_WAYLAND,
                clock_offset_ns: 0,
                max_frame: 256 * 1024,
                devmap: Vec::new(),
            })
        }
        fn connect(&mut self, _mode: u32) -> io::Result<Box<dyn Channel>> {
            Ok(Box::new(ScriptChannel(self.0.clone())))
        }
    }

    use wlwire::proto::op;
    use wlwire::wire::MsgBuilder;

    /// A daemon with one client whose registry offers wl_compositor and
    /// wl_shm, bound as 3 and 4.
    fn scripted(tag: &str) -> (Daemon, Script, UnixStream) {
        let sock = socket_in_tmp(tag);
        let script = Script::new();
        let mut d = Daemon::new(Config::new(&sock), Box::new(ScriptHost(script.clone()))).unwrap();
        let mut client = UnixStream::connect(&sock).unwrap();
        d.turn(100).unwrap();
        use std::io::Write;
        client
            .write_all(
                &MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
                    .new_id(2)
                    .finish(),
            )
            .unwrap();
        d.turn(100).unwrap();
        let hello = frame::Hello {
            version: frame::WL_PROTO_VERSION,
            caps: 0,
        };
        let globals = [
            MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
                .uint(1)
                .string(Some("wl_compositor"))
                .uint(4)
                .finish(),
            MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
                .uint(2)
                .string(Some("wl_shm"))
                .uint(1)
                .finish(),
        ]
        .concat();
        script.push(vec![
            Unit {
                rec: frame::record(frame::REC_HELLO, 0, 0, &hello.encode()),
                descs: Vec::new(),
            },
            Unit {
                rec: frame::record(frame::REC_WAYLAND, 0, 0, &globals),
                descs: Vec::new(),
            },
        ]);
        d.turn(100).unwrap();
        client
            .write_all(
                &[
                    MsgBuilder::new(2, op::wl_registry::REQ_BIND)
                        .uint(1)
                        .generic_new_id("wl_compositor", 4, 3)
                        .finish(),
                    MsgBuilder::new(2, op::wl_registry::REQ_BIND)
                        .uint(2)
                        .generic_new_id("wl_shm", 1, 4)
                        .finish(),
                ]
                .concat(),
            )
            .unwrap();
        d.turn(100).unwrap();
        (d, script, client)
    }

    fn the_client(d: &Daemon) -> &Client {
        d.clients
            .iter()
            .flatten()
            .next()
            .expect("the client is open")
    }

    /// A client committing a large buffer over and over, to a host that is
    /// busy: the daemon holds a frame for it, not the buffer per commit, and
    /// what the client sent after is taken, in order, once the host drains.
    /// Before, the whole of every commit was read and framed at once.
    #[test]
    fn a_client_committing_a_large_buffer_to_a_busy_host_costs_the_daemon_a_frame() {
        let (mut d, script, mut client) = scripted("commit");
        use std::io::Write;
        let (stride, height) = (4096i32, 4096i32); // 16 MiB
        let pool = sys::memfd(c"pool", (stride * height) as u64).unwrap();
        let setup = [
            MsgBuilder::new(3, op::wl_compositor::REQ_CREATE_SURFACE)
                .new_id(5)
                .finish(),
            MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
                .new_id(6)
                .int(stride * height)
                .finish(),
            MsgBuilder::new(6, op::wl_shm_pool::REQ_CREATE_BUFFER)
                .new_id(7)
                .int(0)
                .int(stride / 4)
                .int(height)
                .int(stride)
                .uint(0)
                .finish(),
        ]
        .concat();
        sys::send_with_fds(client.as_raw_fd(), &setup, &[pool.as_raw_fd()]).unwrap();
        d.turn(100).unwrap();
        script.busy.store(true, Ordering::Relaxed);
        let mut frames = vec![
            MsgBuilder::new(5, op::wl_surface::REQ_ATTACH)
                .object(7)
                .int(0)
                .int(0)
                .finish(),
            MsgBuilder::new(5, op::wl_surface::REQ_COMMIT).finish(),
        ];
        for _ in 0..3 {
            frames.push(
                MsgBuilder::new(5, op::wl_surface::REQ_DAMAGE)
                    .int(0)
                    .int(0)
                    .int(i32::MAX)
                    .int(i32::MAX)
                    .finish(),
            );
            frames.push(MsgBuilder::new(5, op::wl_surface::REQ_COMMIT).finish());
        }
        let sync = MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(9)
            .finish();
        frames.push(sync.clone());
        client.write_all(&frames.concat()).unwrap();
        for _ in 0..5 {
            d.turn(10).unwrap();
        }
        let c = the_client(&d);
        let held: usize = c.tx.iter().map(|(f, _)| f.len()).sum();
        assert!(
            held <= 2 * 256 * 1024,
            "{held} bytes framed for a busy host"
        );
        assert!(c.engine.input_blocked());
        assert!(
            c.inbuf.ends_with(&sync),
            "what follows waits in the input buffer"
        );
        // The host drains: everything goes, in order, with no more input.
        script.busy.store(false, Ordering::Relaxed);
        for _ in 0..50 {
            d.turn(10).unwrap();
            if the_client(&d).inbuf.is_empty() && the_client(&d).tx.is_empty() {
                break;
            }
        }
        let recs = script.records();
        let synced: usize = recs
            .iter()
            .filter(|r| r.0 == frame::REC_SHM_SYNC)
            .map(|r| r.2.len())
            .sum();
        assert_eq!(synced, 4 * (stride * height) as usize);
        let last = recs
            .iter()
            .rev()
            .find(|r| r.0 == frame::REC_WAYLAND)
            .unwrap();
        assert!(last.2.ends_with(&sync), "the sync went last");
    }

    /// Descriptors sent beside messages that take none are not held for
    /// ever: past libwayland's own ring the client is closed, as libwayland
    /// closes it. Before, the daemon kept every one.
    #[test]
    fn a_client_sending_descriptors_no_request_takes_is_closed() {
        let (mut d, _script, client) = scripted("fds");
        let e = sys::eventfd().unwrap();
        let fds = [e.as_raw_fd(); 28];
        let mut sent = 0;
        let mut id = 100;
        while sent <= wlwire::wire::MAX_FDS_QUEUED {
            let m = MsgBuilder::new(1, op::wl_display::REQ_SYNC)
                .new_id(id)
                .finish();
            id += 1;
            if sys::send_with_fds(client.as_raw_fd(), &m, &fds).is_err() {
                break;
            }
            sent += fds.len();
            d.turn(0).unwrap();
        }
        for _ in 0..10 {
            d.turn(10).unwrap();
        }
        assert!(
            d.clients.iter().all(|c| c.is_none()),
            "the client holding {sent} descriptors is closed"
        );
    }

    /// A client's errors cost the guest journal a bounded number of lines,
    /// and the daemon's totals a bounded list: the same line again is
    /// counted, not repeated, and past the burst lines are dropped and said
    /// to be. Before, every one was printed and kept.
    #[test]
    fn what_clients_cause_is_logged_and_kept_within_bounds() {
        let mut l = LogLimit::new();
        let t0 = Instant::now();
        let mut printed = 0;
        for _ in 0..1000 {
            printed += l.lines("closing: same".into(), t0).len();
        }
        assert_eq!(printed, 1);
        let mut printed = Vec::new();
        for i in 0..1000 {
            printed.extend(l.lines(format!("closing: {i}"), t0));
        }
        assert!(printed.len() <= LOG_BURST as usize + 1, "{}", printed.len());
        assert!(printed[0].contains("999 more times"), "{printed:?}");
        let later = l.lines("closing: after".into(), t0 + LOG_WINDOW);
        assert!(later[0].contains("lines dropped"), "{later:?}");
        assert_eq!(later.last().unwrap(), "closing: after");
        let mut t = Totals::default();
        for i in 0..1000 {
            t.error(format!("{i}"));
        }
        assert_eq!((t.errors.len(), t.error_count), (MAX_ERRORS, 1000));
    }
}
