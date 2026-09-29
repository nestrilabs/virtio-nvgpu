// SPDX-License-Identifier: Apache-2.0
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
//! What the daemon holds for the channel is at most two frames per client
//! (what `take_units_upto` returns for one frame's worth can run a record
//! over), not what the client's input comes to: the engine reads commits' copies and
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
use wlwire::localin::LocalIn;
use wlwire::policy::{LeaseGate, Policy};
use wlwire::sys;

use crate::budget::{self, Owner, Share};
use crate::log::{self, Level};

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
///
/// What its stream sinks hold for its pipes' readers counts too: bytes the
/// client asked for and is not reading, which it can make the host send as
/// fast as a selection's owner writes them (they are also held to the
/// daemon's [`STREAM_BUDGET`]).
const LOCAL_OUT_MAX: usize = 4 << 20;
const STUCK_FOR: Duration = Duration::from_secs(30);

/// What the daemon holds for `c` that `c` has not read (LOCAL_OUT_MAX).
fn unread(c: &Client) -> usize {
    c.engine.local_out_len() + c.engine.stream_held()
}

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
}

/// A [`LogLimit`] per call site, as the backend meters its lines: with one
/// limit for the whole daemon, one client's errors would use up the burst,
/// and the lines other clients caused (or the daemon's own, on accept) would
/// be dropped. A site is a line's fixed part, so what one client causes over
/// and over can hide only more of the same.
struct Logs {
    sites: HashMap<&'static str, LogLimit>,
}

impl Logs {
    fn new() -> Self {
        Self {
            sites: HashMap::new(),
        }
    }

    /// What to print for `line` from `site` at `now`.
    fn lines(&mut self, site: &'static str, line: String, now: Instant) -> Vec<String> {
        self.sites
            .entry(site)
            .or_insert_with(LogLimit::new)
            .lines(line, now)
    }

    /// Print `line` from `site` at `level`, within that site's limit.
    fn say(&mut self, site: &'static str, level: Level, line: String) {
        if !log::enabled(level) {
            return;
        }
        for l in self.lines(site, line, Instant::now()) {
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
    /// Who what the daemon holds for this client is charged to.
    owner: Owner,
    /// What the client sent that its engine has not taken yet.
    input: LocalIn,
    /// Frames the host has not taken yet.
    tx: VecDeque<(Vec<u8>, Vec<Option<OwnedFd>>)>,
    /// Streams in epoll: descriptor → (stream id, events asked for).
    streams: HashMap<RawFd, (u32, u32)>,
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
    log: Logs,
    /// Descriptors clients may hold between them, and how they are shared.
    fd_budget: u64,
    fd_share: Share,
    /// The listener is out of epoll until then: the daemon had no
    /// descriptor to spare for another client.
    listen_paused: Option<Instant>,
    /// What stream sinks hold for readers, over every client.
    stream_bytes: Arc<budget::Bytes>,
    /// For owners the kernel cannot name.
    next_conn: u64,
    /// Normal mode: the socket's lock file, flocked while the daemon runs,
    /// and its path.
    lock: Option<(std::fs::File, PathBuf)>,
}

/// The lock file a socket at `path` is taken under: libwayland's name for
/// it, so the daemon and a compositor in the guest keep off each other's.
fn lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// Take the socket name `path` as libwayland takes one (wl_display_add_socket):
/// lock `<path>.lock` first, and only then remove what is at `path`, a
/// socket its owner left behind. Removing whatever was there and binding
/// would take the socket of a second daemon, or of a compositor on the same
/// name, and the first daemon's exit would remove the second's.
fn take_socket(path: &Path) -> io::Result<(std::fs::File, PathBuf)> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let lp = lock_path(path);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o660)
        .open(&lp)?;
    if lock.try_lock().is_err() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "{} is in use: {} is locked (another nvgpu-wl-guest, or a compositor)",
                path.display(),
                lp.display()
            ),
        ));
    }
    match std::fs::symlink_metadata(path) {
        // A socket (or anything) its owner could write to, as libwayland
        // decides; one that is not is left, and bind says why.
        Ok(m) if m.permissions().mode() & 0o220 != 0 => std::fs::remove_file(path)?,
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok((lock, lp))
}

/// The socket and its lock go with the daemon that holds them, and only
/// with it.
impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some((_, lp)) = &self.lock {
            let _ = std::fs::remove_file(&self.cfg.listen);
            let _ = std::fs::remove_file(lp);
        }
    }
}

/// Descriptors the daemon keeps for itself, outside what clients may hold:
/// stdio, epoll, the listener, the card and render nodes, the export
/// channel, and headroom for the moment an accept has a socket and not yet a
/// channel.
const FD_OWN: u64 = 32;
/// What one read of a client can add past what was counted before it: all
/// one `recvmsg` takes (`wlwire::sys::recv_with_fds`, libwayland's most).
const FD_SLACK: u64 = wlwire::wire::MAX_FDS_PER_SENDMSG as u64;
/// What a client costs before it sends any: its socket and its channel.
const FD_PER_CLIENT: u64 = 2;
/// Bytes stream sinks may hold for readers that have not taken them, over
/// every client (each connection's sinks are granted at most
/// `wlwire::stream::SINK_TOTAL` besides).
pub const STREAM_BUDGET: u64 = 64 << 20;
/// How long the listener rests when the daemon is out of descriptors.
const LISTEN_BACKOFF: Duration = Duration::from_millis(100);

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
        // What clients may hold: the descriptor limit (main raised it to the
        // hard limit), less the daemon's own and one read's worth.
        let (soft, _) = crate::sys::nofile_limit()?;
        let fd_budget = soft.saturating_sub(FD_OWN + FD_SLACK);
        let mut d = Daemon {
            fd_budget,
            fd_share: Share::quarter(fd_budget, 8),
            listen_paused: None,
            stream_bytes: budget::Bytes::new(STREAM_BUDGET),
            next_conn: 0,
            lock: None,
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
            log: Logs::new(),
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
            d.lock = Some(take_socket(&d.cfg.listen)?);
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
        let mut timeout = if busy {
            5.min(max_wait_ms)
        } else {
            max_wait_ms
        };
        if let Some(t) = self.listen_paused {
            // Rounded up: the last part of a millisecond is a wait too.
            let left = t
                .saturating_duration_since(Instant::now())
                .as_micros()
                .div_ceil(1000) as i32;
            timeout = if timeout < 0 { left } else { timeout.min(left) };
        }
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
                            if gone && (!c.tx.is_empty() || c.engine.input_blocked()) {
                                // The client is gone with frames still waiting
                                // for a busy host, or with input the engine
                                // is not taking yet. read_local will not read
                                // then, so the EOF that closes the slot would
                                // never be seen, and HUP cannot be masked:
                                // every epoll_wait would return at once and
                                // retry the SEND, at 100% CPU and a virtqueue
                                // round trip each, until the host's compositor
                                // drained -- or for good, had the engine's
                                // count of its backlog ever outlived the
                                // backlog. Nobody
                                // is left to read the replies to those frames,
                                // and closing the channel ends the host's side
                                // of the client anyway (libwayland-server
                                // destroys a client on HUP too, unread input
                                // and all).
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
                            if let Some(&(id, _)) = c.streams.get(&fd) {
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
        self.resume_listening();
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

    fn add_client(&mut self, sock: UnixStream, chan: Box<dyn Channel>, local: Local, owner: Owner) {
        if sock.set_nonblocking(true).is_err() {
            return;
        }
        let mut engine = Engine::new(self.engine_config(local));
        // A guest compositor's output is paced by the channel too (export
        // mode): its peer is the host client, behind the channel.
        engine.set_input_limit(Some(wlwire::engine::CHANNEL_HIGH_WATER));
        // What stream sinks hold for the client's readers comes out of the
        // daemon's memory, and so out of one pool with a share per owner
        // (the backend does the same with its queue budget).
        engine.set_stream_budget(self.stream_bytes.for_owner(owner));
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
        {
            return;
        }
        if epoll_ctl(
            ep,
            libc::EPOLL_CTL_ADD,
            chan.poll_fd(),
            libc::EPOLLIN as u32,
            base | SUB_CHAN,
        )
        .is_err()
        {
            let _ = epoll_ctl(ep, libc::EPOLL_CTL_DEL, sock.as_raw_fd(), 0, 0);
            return;
        }
        self.clients[slot] = Some(Client {
            sock,
            chan,
            engine,
            owner,
            input: LocalIn::default(),
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

    /// Descriptors every client holds between them, and those `o`'s clients
    /// hold: each client's socket and channel, what it sent that no request
    /// has taken yet, what waits for the host, and what its engine holds.
    fn fds_in_use(&self, o: Option<Owner>) -> (u64, u64) {
        let (mut all, mut mine) = (0, 0);
        for c in self.clients.iter().flatten() {
            let tx: usize = c.tx.iter().map(|(_, f)| f.iter().flatten().count()).sum();
            let n = FD_PER_CLIENT + (c.input.fds() + tx + c.engine.held_fds()) as u64;
            all += n;
            if Some(c.owner) == o {
                mine += n;
            }
        }
        (all, mine)
    }

    /// May `o` hold `n` more descriptors?
    fn fds_admit(&self, o: Owner, n: u64) -> Result<(), (budget::Over, u64)> {
        let (all, mine) = self.fds_in_use(Some(o));
        budget::admits(&self.fd_share, mine, n, all, self.fd_budget).map_err(|w| (w, mine))
    }

    /// Stop accepting for a while: out of descriptors, every accept fails at
    /// once and the pending connection stays in the backlog, so the listener
    /// would wake every wait.
    fn pause_listening(&mut self) {
        if self.listen_paused.is_some() {
            return;
        }
        if let Some(l) = &self.listener {
            let _ = epoll_ctl(
                self.ep.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                l.as_raw_fd(),
                0,
                0,
            );
        }
        self.listen_paused = Some(Instant::now() + LISTEN_BACKOFF);
    }

    fn resume_listening(&mut self) {
        if self.listen_paused.is_none_or(|t| Instant::now() < t) {
            return;
        }
        self.listen_paused = None;
        if let Some(l) = &self.listener {
            let _ = epoll_ctl(
                self.ep.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                l.as_raw_fd(),
                libc::EPOLLIN as u32,
                TOK_LISTENER,
            );
        }
    }

    /// Turn a client away before it has a channel, telling it why.
    fn refuse(s: &UnixStream, code: u32, why: String) {
        let err = Fatal::new(Blame::Remote, 1, code, why);
        let _ = sys::send_with_fds(s.as_raw_fd(), &err.display_error(), &[]);
    }

    fn accept(&mut self) {
        loop {
            if self.listen_paused.is_some() {
                return;
            }
            // Room for one more client at all: otherwise it waits in the
            // backlog until some other client lets go of descriptors.
            if self.fds_in_use(None).0 + FD_PER_CLIENT > self.fd_budget {
                self.log.say(
                    "fd budget",
                    Level::Warn,
                    format!(
                        "nvgpu-wl-guest: clients hold the daemon's {} descriptors; \
                         not accepting for now",
                        self.fd_budget
                    ),
                );
                self.pause_listening();
                return;
            }
            let Some(l) = &self.listener else { return };
            let s = match l.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) => {
                    // Out of descriptors or memory, the pending connection
                    // stays in the backlog and every accept fails at once:
                    // the listener rests rather than spin, and the line is
                    // metered like any a client can cause.
                    self.log.say(
                        "accept",
                        Level::Warn,
                        format!("nvgpu-wl-guest: accept: {e}"),
                    );
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
                    ) {
                        self.pause_listening();
                    }
                    return;
                }
            };
            // The client's process, held by a pidfd (SO_PEERPIDFD), which
            // names it and never a later process given its pid.
            let (owner, pidfd) = match peer_pid(&s) {
                Some(pid) => match crate::sys::peer_pidfd(s.as_raw_fd(), pid) {
                    // Gone already (a kernel may give a pidfd for a process
                    // that has exited): nobody to charge or serve.
                    Ok(fd) if crate::sys::pidfd_exited(fd.as_raw_fd()) => continue,
                    Ok(fd) => (Owner::Pid(pid), Some(fd)),
                    // Gone already: nobody to charge, and nobody to serve.
                    Err(e) if e.raw_os_error() == Some(libc::ESRCH) => continue,
                    // A kernel without pidfds: the pid alone, as before.
                    Err(_) => (Owner::Pid(pid), None),
                },
                None => {
                    self.next_conn += 1;
                    (Owner::Conn(self.next_conn), None)
                }
            };
            if let Err((why, held)) = self.fds_admit(owner, FD_PER_CLIENT) {
                self.log.say(
                    "fd share",
                    Level::Warn,
                    format!(
                        "nvgpu-wl-guest: {owner:?} holds {held} of the daemon's {} descriptors \
                         ({why:?}); refusing its connection",
                        self.fd_budget
                    ),
                );
                Self::refuse(
                    &s,
                    wlwire::engine::ERR_NO_MEMORY,
                    "virtio-nvgpu: too many descriptors held".into(),
                );
                continue;
            }
            // Charged to the client, not to the daemon: each guest process
            // holds only a share of the VM's channels and their budgets
            // (NVGPU_WL_IOC_CONNECT_FOR). The kernel finds the process by
            // its pid, so what it charged is the client's only if the client
            // was still there after: a pid is given to another process only
            // once its own has exited. A client that has, whichever process
            // was charged, is dropped.
            let ch = match owner {
                Owner::Pid(pid) => self.conn.connect_for(pid),
                Owner::Conn(_) => self.conn.connect(uapi::CONNECT),
            };
            let gone = pidfd
                .as_ref()
                .is_some_and(|p| crate::sys::pidfd_exited(p.as_raw_fd()));
            match ch {
                Ok(_) if gone => {}
                Ok(ch) => self.add_client(s, ch, Local::Client, owner),
                Err(e) => {
                    self.log.say(
                        "host channel",
                        Level::Warn,
                        format!("nvgpu-wl-guest: cannot open a channel to the host: {e}"),
                    );
                    Self::refuse(
                        &s,
                        wlwire::engine::ERR_IMPLEMENTATION,
                        format!("virtio-nvgpu: no host channel: {e}"),
                    );
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
            // Without waiting: a guest compositor whose backlog is full
            // would otherwise stall this thread, and every client with it.
            // The host client it was for is turned away rather than kept
            // waiting.
            match crate::sys::connect_nonblocking(&target) {
                Ok(s) => {
                    // Each is a host client, which the guest kernel cannot
                    // name: an owner of its own.
                    self.next_conn += 1;
                    let owner = Owner::Conn(self.next_conn);
                    self.add_client(s, ch, Local::Server, owner)
                }
                Err(e) => self.log.say(
                    "export connect",
                    Level::Warn,
                    format!(
                        "nvgpu-wl-guest: cannot reach the guest compositor at {}: {e}",
                        target.display()
                    ),
                ),
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
        self.log.say(
            "closing",
            Level::Warn,
            format!(
                "nvgpu-wl-guest: closing a connection: {:?}: {text:?}",
                f.blame
            ),
        );
        // Our client gets the error it earned, or the host's verdict, after
        // what it already has (Engine::end_with): written straight to the
        // socket after a flush that stopped inside an event, it would land
        // in the middle of that event. What the socket does not take now
        // goes when the slot closes (sync).
        let record = c.engine.end_with(&f);
        let _ = c.engine.local_out().flush(c.sock.as_raw_fd());
        // The guest compositor broke the protocol: tell the host client.
        if !c.engine.local_is_client() && f.blame == Blame::Local {
            let mut q = VecDeque::from([record]);
            let (mut fr, fds) = frame::pack(&mut q, self.info.max_frame, frame::MAX_DESC, false);
            let _ = c.chan.send(&mut fr, &fds);
        }
        c.tx.clear();
        c.closing = true;
    }

    fn read_local(&mut self, slot: usize) {
        let mut buf = vec![0u8; 64 * 1024];
        // A few reads a turn, not until the socket is empty: a client that
        // writes as fast as it is read would otherwise have a megabyte of
        // requests taken before the channel is read again, and the host's
        // replies -- and the deletes that free the ids those requests take
        // -- wait behind them. Level-triggered, the rest is next turn's.
        for _ in 0..4 {
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
                    let got = fds.len();
                    // Past libwayland's descriptor ring the client is
                    // closed, as libwayland closes it (LocalIn::push).
                    let ring = c.input.push(&buf[..n], fds);
                    let owner = c.owner;
                    // Descriptors come with the read, before anything can
                    // refuse them: counted now, and past the owner's share
                    // of the daemon's (budget.rs) the client is closed too.
                    // One read adds at most FD_SLACK, which the budget
                    // leaves room for.
                    let over = if got > 0 {
                        self.fds_admit(owner, 0).err()
                    } else {
                        None
                    };
                    let refused = match (over, ring) {
                        (Some((why, held)), _) => Some(Fatal::no_memory(
                            Blame::Local,
                            format!(
                                "the client's process holds {held} of the proxy's {} \
                                 descriptors ({why:?})",
                                self.fd_budget
                            ),
                        )),
                        (None, ring) => ring.err(),
                    };
                    if let Some(f) = refused {
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
            if c.closing || !c.tx.is_empty() || c.input.is_empty() {
                return;
            }
            let before = c.input.len();
            if let Err(f) = c.engine.from_local(&mut c.input, &mut GuestPlat) {
                self.fatal(slot, f);
                return;
            }
            let c = self.clients[slot].as_mut().unwrap();
            // Only part of a message is left, or nothing was taken.
            if c.input.len() == before && !c.engine.has_channel_output() {
                return;
            }
        }
    }

    fn write_local(&mut self, slot: usize) {
        let c = self.clients[slot].as_mut().unwrap();
        if let Err(e) = c.engine.local_out().flush(c.sock.as_raw_fd()) {
            self.log.say(
                "write",
                Level::Warn,
                format!("nvgpu-wl-guest: writing to a client: {e}"),
            );
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
                    self.log.say(
                        "host refused",
                        Level::Warn,
                        format!("nvgpu-wl-guest: the host refused a frame: {e}"),
                    );
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
            // One being closed gets nothing more at all: whatever it was
            // told last (a wl_display.error) stays the last thing it reads.
            if c.closing || unread(c) >= LOCAL_OUT_MAX {
                return;
            }
            let r = match c.chan.recv(max, card, render) {
                Ok(r) => r,
                Err(e) => {
                    self.log.say(
                        "channel",
                        Level::Warn,
                        format!("nvgpu-wl-guest: channel: {e}"),
                    );
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
                    self.log.say(
                        "write",
                        Level::Warn,
                        format!("nvgpu-wl-guest: writing to a client: {e}"),
                    );
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
        // Nothing of the client's is read while frames wait for the host, or
        // while the engine takes no more input (read_local), so nothing that
        // says "readable" is asked for then either: not EPOLLIN, and not
        // EPOLLRDHUP, which a client that shut down only its writing side
        // would report on every wait. A client that is gone altogether still
        // wakes us with EPOLLHUP, which cannot be masked (turn).
        let mut want = 0;
        if c.tx.is_empty() && !c.engine.input_blocked() {
            want |= (libc::EPOLLIN | libc::EPOLLRDHUP) as u32;
        }
        if c.engine.local_out_len() > 0 {
            want |= libc::EPOLLOUT as u32;
        }
        // The channel is read only while the client keeps up (W2), and a
        // client that stays that far behind is dropped, as a compositor
        // drops a client it cannot write to.
        let behind = unread(c) >= LOCAL_OUT_MAX;
        let chan_want = if behind { 0 } else { libc::EPOLLIN as u32 };
        if behind {
            let since = *c.stuck_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= STUCK_FOR {
                self.log.say(
                    "stuck",
                    Level::Warn,
                    format!(
                        "nvgpu-wl-guest: a client has not read {} bytes in {}s; closing it",
                        unread(c),
                        STUCK_FOR.as_secs()
                    ),
                );
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
        sync_streams(ep, base, c);
    }

    fn close(&mut self, slot: usize) {
        if let Some(mut c) = self.clients[slot].take() {
            self.totals.lock().unwrap().add(&c.engine);
            // Everything of the client's leaves epoll while its descriptor is
            // still open: closing is not enough when the file is shared, as
            // a stream's is with the client that sent it (and a channel's
            // readiness descriptor may be, for another Channel than
            // /dev/nvgpu-wl's).
            let ep = self.ep.as_raw_fd();
            let fds = [c.sock.as_raw_fd(), c.chan.poll_fd()];
            for fd in fds.into_iter().chain(c.streams.drain().map(|(fd, _)| fd)) {
                let _ = epoll_ctl(ep, libc::EPOLL_CTL_DEL, fd, 0, 0);
            }
        }
    }
}

/// Bring `c`'s streams in epoll up to date with what its engine wants.
///
/// A stream is watched only while the engine wants to read or write it,
/// level-triggered, and taken out of epoll otherwise: epoll reports an error
/// or a hangup whatever it was asked for, so a sink whose reader went away
/// with nothing to write, or a source with no credit whose writer went away,
/// would otherwise wake the daemon on every wait until the far side moved.
/// Streams the engine has ended are taken out before their descriptors
/// close: a sink's descriptor shares its open file with the client that sent
/// it, and a registration left behind would outlive the close.
fn sync_streams(ep: RawFd, base: u64, c: &mut Client) {
    for fd in c.engine.take_closed_streams() {
        if c.streams.remove(&fd.as_raw_fd()).is_some() {
            let _ = epoll_ctl(ep, libc::EPOLL_CTL_DEL, fd.as_raw_fd(), 0, 0);
        }
    }
    let mut now = HashMap::new();
    for i in c.engine.stream_interest() {
        let mut ev = 0;
        if i.read {
            ev |= libc::EPOLLIN as u32;
        }
        if i.write {
            ev |= libc::EPOLLOUT as u32;
        }
        let tok = base | SUB_STREAM | i.fd as u64;
        let was = c.streams.remove(&i.fd).map(|(_, e)| e);
        let op = match (was, ev) {
            (None, 0) => None,
            (Some(_), 0) => Some(libc::EPOLL_CTL_DEL),
            (None, _) => Some(libc::EPOLL_CTL_ADD),
            (Some(w), _) if w == ev => None,
            (Some(_), _) => Some(libc::EPOLL_CTL_MOD),
        };
        let ok = match op {
            Some(op) => epoll_ctl(ep, op, i.fd, ev, tok).is_ok(),
            None => true,
        };
        if ev != 0 && ok {
            now.insert(i.fd, (i.id, ev));
        }
    }
    // Every stream the engine ends comes back through take_closed_streams,
    // so nothing registered is left over. Were anything, its number could
    // name another file by now, and is not touched.
    debug_assert!(c.streams.is_empty(), "a stream left epoll unseen");
    c.streams = now;
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
            c.input.bytes().ends_with(&sync),
            "what follows waits in the input buffer"
        );
        // The host drains: everything goes, in order, with no more input.
        script.busy.store(false, Ordering::Relaxed);
        for _ in 0..50 {
            d.turn(10).unwrap();
            if the_client(&d).input.is_empty() && the_client(&d).tx.is_empty() {
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

    /// Turns the daemon makes in `ms` with nothing to do: an idle one waits
    /// out each turn's 100 ms, a spinning one returns at once.
    fn turns_in(d: &mut Daemon, ms: u128) -> usize {
        let t = Instant::now();
        let mut n = 0;
        while t.elapsed().as_millis() < ms {
            d.turn(100).unwrap();
            n += 1;
        }
        n
    }

    /// A client that truncates its own pool before a commit's copy is read
    /// gets a short copy and goes on: what it sends after is taken, the
    /// daemon idles, and when it hangs up its slot and channel close. Before,
    /// the copy's unread rest stayed counted for good (wlwire's S2): its
    /// input was never read again, every epoll_wait returned at once, and
    /// the slot, the channel and the host's compositor client outlived it.
    #[test]
    fn a_client_that_truncates_its_pool_is_not_wedged_and_is_closed_when_it_goes() {
        let (mut d, script, mut client) = scripted("truncate");
        use std::io::Write;
        let (stride, height) = (4096i32, 2048i32); // 8 MiB
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
        sys::ftruncate(pool.as_raw_fd(), 0).unwrap();
        let sync = MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(9)
            .finish();
        client
            .write_all(
                &[
                    MsgBuilder::new(5, op::wl_surface::REQ_ATTACH)
                        .object(7)
                        .int(0)
                        .int(0)
                        .finish(),
                    MsgBuilder::new(5, op::wl_surface::REQ_COMMIT).finish(),
                    sync.clone(),
                ]
                .concat(),
            )
            .unwrap();
        for _ in 0..3 {
            d.turn(50).unwrap();
        }
        let c = the_client(&d);
        let wedged = !c.input.is_empty() || c.engine.input_blocked();
        let recs = script.records();
        let last = recs
            .iter()
            .rev()
            .find(|r| r.0 == frame::REC_WAYLAND)
            .unwrap();
        let synced = last.2.ends_with(&sync);
        let idle = turns_in(&mut d, 300);
        drop(client);
        let after = turns_in(&mut d, 300);
        // The daemon's own part first: whatever the engine's count says, a
        // client whose input is not taken is not polled for it, and one
        // that hangs up is closed.
        assert!(idle < 30, "{idle} turns in 300 ms: the daemon spins");
        assert!(after < 30, "{after} turns in 300 ms after the client went");
        assert!(
            d.clients.iter().all(|c| c.is_none()),
            "the slot of a client that hung up is closed"
        );
        assert!(!wedged, "the client's input is no longer taken");
        assert!(synced, "the sync after the commit reached the host");
    }

    /// A daemon with one client that has a data device (5) and a data offer
    /// (0xff000000) from the host.
    fn with_an_offer(tag: &str) -> (Daemon, Script, UnixStream) {
        let sock = socket_in_tmp(tag);
        let script = Script::new();
        let mut d = Daemon::new(Config::new(&sock), Box::new(ScriptHost(script.clone()))).unwrap();
        let mut client = UnixStream::connect(&sock).unwrap();
        d.turn(50).unwrap();
        use std::io::Write;
        client
            .write_all(
                &MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
                    .new_id(2)
                    .finish(),
            )
            .unwrap();
        d.turn(50).unwrap();
        let hello = frame::Hello {
            version: frame::WL_PROTO_VERSION,
            caps: frame::HELLO_STREAM_WINDOW,
        };
        let globals = [
            MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
                .uint(1)
                .string(Some("wl_seat"))
                .uint(1)
                .finish(),
            MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
                .uint(2)
                .string(Some("wl_data_device_manager"))
                .uint(3)
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
        d.turn(50).unwrap();
        client
            .write_all(
                &[
                    MsgBuilder::new(2, op::wl_registry::REQ_BIND)
                        .uint(1)
                        .generic_new_id("wl_seat", 1, 3)
                        .finish(),
                    MsgBuilder::new(2, op::wl_registry::REQ_BIND)
                        .uint(2)
                        .generic_new_id("wl_data_device_manager", 3, 4)
                        .finish(),
                    MsgBuilder::new(4, op::wl_data_device_manager::REQ_GET_DATA_DEVICE)
                        .new_id(5)
                        .object(3)
                        .finish(),
                ]
                .concat(),
            )
            .unwrap();
        d.turn(50).unwrap();
        let offer = MsgBuilder::new(5, op::wl_data_device::EVT_DATA_OFFER)
            .new_id(0xff00_0000)
            .finish();
        script.push(vec![Unit {
            rec: frame::record(frame::REC_WAYLAND, 0, 0, &offer),
            descs: Vec::new(),
        }]);
        d.turn(50).unwrap();
        (d, script, client)
    }

    /// `wl_data_offer.receive` into a pipe whose reader then goes away,
    /// with nothing from the host to write: the pipe reports EPOLLERR,
    /// which epoll always reports, for a stream the engine wants nothing of.
    /// The daemon stops watching such a stream rather than waking for it.
    /// Before, it re-armed every stream on every turn it touched, and spun
    /// until the host's source sent something.
    #[test]
    fn a_stream_whose_reader_is_gone_does_not_spin_the_daemon() {
        let (mut d, _script, client) = with_an_offer("sink");
        let (rd, wr) = sys::pipe().unwrap();
        let recv = MsgBuilder::new(0xff00_0000, op::wl_data_offer::REQ_RECEIVE)
            .string(Some("text/plain"))
            .finish();
        sys::send_with_fds(client.as_raw_fd(), &recv, &[wr.as_raw_fd()]).unwrap();
        drop(wr);
        d.turn(50).unwrap();
        d.turn(50).unwrap();
        assert_eq!(the_client(&d).engine.stream_interest().len(), 1);
        let before = turns_in(&mut d, 300);
        assert!(
            before < 30,
            "{before} turns in 300 ms before the reader went"
        );
        drop(rd);
        let after = turns_in(&mut d, 300);
        assert!(after < 30, "{after} turns in 300 ms: the daemon spins");
    }

    /// The streams of a client that goes are taken out of epoll before
    /// their descriptors close. A sink's descriptor shares its open file
    /// with the process that sent it, so a registration left behind would
    /// outlive the slot: it would keep reporting (a pipe with room is
    /// writable), and to whichever client the slot went to next.
    #[test]
    fn the_streams_of_a_client_that_goes_leave_nothing_in_epoll() {
        let (mut d, script, client) = with_an_offer("sinkgone");
        let (rd, wr) = sys::pipe().unwrap();
        let recv = MsgBuilder::new(0xff00_0000, op::wl_data_offer::REQ_RECEIVE)
            .string(Some("text/plain"))
            .finish();
        sys::send_with_fds(client.as_raw_fd(), &recv, &[wr.as_raw_fd()]).unwrap();
        // The client keeps its copy of the write end, and so the pipe's
        // open file, as a process that forked would.
        d.turn(50).unwrap();
        // The host sends more than the pipe holds, and nobody reads it: the
        // sink is watched for writing.
        let id = the_client(&d).engine.stream_interest()[0].id;
        let data = |_| Unit {
            rec: frame::record(frame::REC_STREAM_DATA, id, 0, &[7u8; 60_000]),
            descs: Vec::new(),
        };
        script.push((0..2).map(data).collect());
        d.turn(50).unwrap();
        assert!(the_client(&d).streams.values().any(|&(_, ev)| ev != 0));
        drop(client);
        for _ in 0..3 {
            d.turn(50).unwrap();
        }
        assert!(d.clients.iter().all(|c| c.is_none()));
        let turns = turns_in(&mut d, 300);
        assert!(turns < 30, "{turns} turns in 300 ms after the client went");
        assert_eq!(watched(&d), 1, "only the listener is left in epoll");
        drop((rd, wr));
    }

    /// What stream sinks hold for a client that is not reading its pipe is
    /// charged to the client's process in the daemon's stream budget, and
    /// counts as unread for the stuck-client rule; it is given back when
    /// the stream ends. Before, the daemon's sinks were unbudgeted: about
    /// 15 MiB per connection, and a process could hold 16 connections.
    #[test]
    fn what_sinks_hold_is_charged_to_the_clients_process() {
        let (mut d, script, client) = with_an_offer("sinkbudget");
        let (rd, wr) = sys::pipe().unwrap();
        let recv = MsgBuilder::new(0xff00_0000, op::wl_data_offer::REQ_RECEIVE)
            .string(Some("text/plain"))
            .finish();
        sys::send_with_fds(client.as_raw_fd(), &recv, &[wr.as_raw_fd()]).unwrap();
        drop(wr);
        d.turn(50).unwrap();
        let id = the_client(&d).engine.stream_interest()[0].id;
        let data = |_| Unit {
            rec: frame::record(frame::REC_STREAM_DATA, id, 0, &[7u8; 60_000]),
            descs: Vec::new(),
        };
        script.push((0..2).map(data).collect());
        d.turn(50).unwrap();
        let c = the_client(&d);
        let held = c.engine.stream_held();
        assert!(held > 0, "the pipe is full and the rest is held");
        assert_eq!(d.stream_bytes.used(c.owner), (held as u64, held as u64));
        assert_eq!(unread(c), c.engine.local_out_len() + held);
        let owner = c.owner;
        // The reader goes: the sink ends, and gives back what it held.
        drop(rd);
        for _ in 0..3 {
            d.turn(50).unwrap();
        }
        assert_eq!(d.stream_bytes.used(owner), (0, 0));
    }

    /// Registrations in the daemon's epoll set (its fdinfo's `tfd:` lines).
    fn watched(d: &Daemon) -> usize {
        std::fs::read_to_string(format!("/proc/self/fdinfo/{}", d.ep.as_raw_fd()))
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("tfd:"))
            .count()
    }

    /// Whole messages of `got` as the client's libwayland would parse them,
    /// each a registry global or `wl_display.error`; the stream may end
    /// inside the last one, when the daemon closed a client whose socket
    /// was full. The number of errors.
    fn globals_then_an_error(got: &[u8]) -> usize {
        let mut p = got;
        let mut errors = 0;
        while let Some(h) = wlwire::wire::peek_header(p) {
            if (h.size as usize) > p.len() {
                break;
            }
            let (m, rest) = p.split_at(h.size as usize);
            p = rest;
            let ifc = match (h.object, h.opcode) {
                (2, op::wl_registry::EVT_GLOBAL) => wlwire::proto::WL_REGISTRY,
                (1, op::wl_display::EVT_ERROR) => wlwire::proto::WL_DISPLAY,
                _ => panic!(
                    "a message on {}/{}: the stream is corrupt",
                    h.object, h.opcode
                ),
            };
            assert_eq!(errors, 0, "a message after the error");
            let desc =
                &wlwire::proto::iface(ifc).messages(wlwire::proto::Dir::Event)[h.opcode as usize];
            let args = wlwire::wire::parse(desc, m).expect("a whole message that parses");
            if ifc == wlwire::proto::WL_DISPLAY {
                errors += 1;
                assert_eq!(args[1].val, wlwire::wire::Val::Uint(0), "invalid_object");
            } else {
                // What `scripted` offered, and the flood.
                let name = args[1].val;
                assert!(
                    [&b"wl_compositor"[..], b"wl_shm"]
                        .iter()
                        .any(|n| name == wlwire::wire::Val::Str(Some(n))),
                    "the stream is corrupt: {name:?}"
                );
            }
        }
        errors
    }

    /// A client that breaks the protocol while the daemon is part-way
    /// through writing it an event gets the error after the last whole
    /// event, never in the middle of one. Before, the error was written
    /// straight to the socket after a flush that could stop inside an
    /// event, and a client reading at the same time -- which frees the
    /// room the error then fits in -- read it as that event's arguments.
    /// The race is between two system calls of the daemon, so it is tried
    /// a number of times.
    #[test]
    fn a_fatal_error_never_lands_in_the_middle_of_an_event() {
        use std::io::{Read, Write};
        let global = |n: u32| {
            MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
                .uint(n)
                .string(Some("wl_compositor"))
                .uint(4)
                .finish()
        };
        // About 400 KiB of events: more than the socket holds.
        let flood = || -> Vec<Unit> {
            (0..25)
                .map(|r| Unit {
                    rec: frame::record(
                        frame::REC_WAYLAND,
                        0,
                        0,
                        &(0..512)
                            .map(|i| global(100 + r * 512 + i))
                            .collect::<Vec<_>>()
                            .concat(),
                    ),
                    descs: Vec::new(),
                })
                .collect()
        };
        let mut delivered = 0;
        for round in 0..40 {
            let (mut d, script, client) = scripted(&format!("midmsg{round}"));
            let mut reader = client.try_clone().unwrap();
            reader
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let t = std::thread::spawn(move || {
                let mut got = Vec::new();
                let _ = reader.read_to_end(&mut got);
                got
            });
            script.push(flood());
            for _ in 0..round % 3 {
                d.turn(0).unwrap();
            }
            (&client)
                .write_all(&MsgBuilder::new(99, 0).finish())
                .unwrap();
            for _ in 0..100 {
                d.turn(5).unwrap();
                if d.clients.iter().all(|c| c.is_none()) {
                    break;
                }
            }
            assert!(d.clients.iter().all(|c| c.is_none()));
            drop(client);
            delivered += globals_then_an_error(&t.join().unwrap());
        }
        assert!(delivered > 0, "the error reached some of the clients");
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

    /// One call site's burst is its own: a client whose errors use up the
    /// limit of the line that closes connections hides no other line. Before,
    /// one limit for the whole daemon dropped every other line too, the
    /// accept failures that say the daemon is out of descriptors among
    /// them.
    #[test]
    fn one_call_sites_lines_do_not_crowd_out_anothers() {
        let mut l = Logs::new();
        let t0 = Instant::now();
        for i in 0..100 {
            l.lines("closing", format!("closing: {i}"), t0);
        }
        assert!(l.lines("closing", "closing: more".into(), t0).is_empty());
        assert_eq!(
            l.lines("accept", "accept: out of descriptors".into(), t0),
            vec!["accept: out of descriptors".to_string()]
        );
    }

    /// A child of this test process, run for `role` against `sock`: it
    /// connects, and exits at once or when killed.
    const CHILD: &str = "NVWL_DAEMON_TEST_CHILD";

    #[test]
    fn a_child_that_connects() {
        let Ok(v) = std::env::var(CHILD) else { return };
        let (role, sock) = v.split_once(':').unwrap();
        let _s = UnixStream::connect(sock).unwrap();
        if role == "stay" {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    fn child(role: &str, sock: &Path) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "daemon::tests::a_child_that_connects"])
            .env(CHILD, format!("{role}:{}", sock.display()))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// A host that records what the daemon opens, and runs `during` in the
    /// middle of a CONNECT_FOR.
    struct Recording {
        opened: Arc<Mutex<Vec<String>>>,
        during: Arc<Mutex<Option<std::process::Child>>>,
    }

    impl Connector for Recording {
        fn info(&mut self) -> io::Result<HostInfo> {
            BusyHost {
                max_frame: 256 * 1024,
                ..Default::default()
            }
            .info()
        }
        fn connect(&mut self, mode: u32) -> io::Result<Box<dyn Channel>> {
            self.opened.lock().unwrap().push(format!("connect {mode}"));
            BusyHost::default().connect(mode)
        }
        fn connect_for(&mut self, pid: i32) -> io::Result<Box<dyn Channel>> {
            self.opened
                .lock()
                .unwrap()
                .push(format!("connect_for {pid}"));
            // The client's process goes while the kernel looks it up.
            if let Some(mut c) = self.during.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            BusyHost::default().connect(uapi::CONNECT)
        }
    }

    /// A client whose process is gone by the time the daemon asks the
    /// kernel for its channel -- before, or during -- is dropped, and no
    /// channel is left charged to the daemon or to a process that reused
    /// its pid. Before, CONNECT_FOR was asked for by pid number alone, and
    /// a process the kernel could not find fell back to a plain CONNECT
    /// charged to the daemon.
    #[test]
    fn a_client_whose_process_is_gone_gets_no_channel() {
        let sock = socket_in_tmp("gone");
        let opened = Arc::new(Mutex::new(Vec::new()));
        let during = Arc::new(Mutex::new(None));
        let mut d = Daemon::new(
            Config::new(&sock),
            Box::new(Recording {
                opened: opened.clone(),
                during: during.clone(),
            }),
        )
        .unwrap();
        // Gone before the accept.
        assert!(child("go", &sock).wait().unwrap().success());
        d.turn(100).unwrap();
        assert!(opened.lock().unwrap().is_empty(), "{:?}", opened.lock());
        assert!(d.clients.iter().all(|c| c.is_none()));
        // Gone during CONNECT_FOR: asked for, and let go of.
        let c = child("stay", &sock);
        let pid = c.id();
        *during.lock().unwrap() = Some(c);
        for _ in 0..50 {
            d.turn(20).unwrap();
            if !opened.lock().unwrap().is_empty() {
                break;
            }
        }
        assert_eq!(*opened.lock().unwrap(), vec![format!("connect_for {pid}")]);
        assert!(
            d.clients.iter().all(|c| c.is_none()),
            "the client was dropped"
        );
    }

    /// A second daemon on a socket name one already serves is refused, and
    /// the first keeps its socket; the socket and its lock go with the
    /// daemon that holds them, and a socket left behind by one that was
    /// killed is taken over. Before, the second removed the first's socket
    /// and bound its own.
    #[test]
    fn a_socket_name_is_taken_under_its_lock() {
        let sock = socket_in_tmp("lock");
        let host = || {
            Box::new(BusyHost {
                max_frame: 256 * 1024,
                ..Default::default()
            })
        };
        let mut first = Daemon::new(Config::new(&sock), host()).unwrap();
        let e = Daemon::new(Config::new(&sock), host())
            .err()
            .expect("refused");
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse, "{e}");
        let _c = UnixStream::connect(&sock).unwrap();
        first.turn(100).unwrap();
        assert_eq!(
            first.snapshot().clients,
            1,
            "the first still has its socket"
        );
        drop(first);
        assert!(!sock.exists() && !lock_path(&sock).exists());
        // One killed without removing its socket.
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        assert!(sock.exists());
        let mut again = Daemon::new(Config::new(&sock), host()).unwrap();
        let _c = UnixStream::connect(&sock).unwrap();
        again.turn(100).unwrap();
        assert_eq!(again.snapshot().clients, 1);
    }

    /// A host whose export socket always has a client waiting.
    struct ExportHost;

    impl Connector for ExportHost {
        fn info(&mut self) -> io::Result<HostInfo> {
            Ok(HostInfo {
                caps: uapi::CAP_EXPORT,
                clock_offset_ns: 0,
                max_frame: 256 * 1024,
                devmap: Vec::new(),
            })
        }
        fn connect(&mut self, mode: u32) -> io::Result<Box<dyn Channel>> {
            let ready = sys::eventfd()?;
            if mode == uapi::LISTEN {
                sys::eventfd_signal(ready.as_raw_fd());
            }
            Ok(Box::new(BusyChannel {
                sends: Default::default(),
                asked: Default::default(),
                ready,
            }))
        }
    }

    /// Export mode, a guest compositor whose backlog is full: the host
    /// client is turned away and the daemon goes on. Before, the connect
    /// blocked the daemon's only thread until the compositor accepted, and
    /// every other client with it.
    #[test]
    fn a_guest_compositor_with_a_full_backlog_does_not_stall_the_daemon() {
        let target = socket_in_tmp("full");
        let own = socket_in_tmp("fullown");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _l = crate::sys::listen_unix(&target, 0).unwrap();
            // The one connection a backlog of 0 holds.
            let _first = UnixStream::connect(&target).unwrap();
            let mut cfg = Config::new(&own);
            cfg.export_to = Some(target.clone());
            let mut d = Daemon::new(cfg, Box::new(ExportHost)).unwrap();
            d.turn(100).unwrap();
            let _ = tx.send(d.snapshot().clients);
        });
        let clients = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the daemon's turn came back");
        assert_eq!(clients, 0, "the host client was turned away");
    }
}
