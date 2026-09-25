//! One proxied connection to the host compositor.
//!
//! **Eager draining.** Hyprland gives each client a 1 MiB output buffer and
//! disconnects a client whose buffer fills (`Compositor.cpp:306`,
//! `wayland-server.c:228-256`). The guest reads only when it gets round to a
//! WL_RECV, so the connection cannot be read on the guest's schedule: a reader
//! thread per connection reads the socket as soon as it is readable, runs the
//! engine, and queues the translated records here until the guest takes them.
//! The queue is bounded (`max_queue`); a guest that stops reading altogether
//! loses the connection rather than the backend its memory.
//!
//! **Backpressure the other way** is the guest's: WL_SEND is refused with
//! `EAGAIN` while more than `max_backlog` bytes wait for a compositor that is
//! not reading, and the daemon stops reading its client until the backlog
//! drains -- the client's own libwayland buffer is where it waits.
//!
//! **Readiness** is an eventfd, readable while anything is queued for the
//! guest (records, or the HANGUP that ends the connection).

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use wlwire::engine::{Blame, Engine, EngineConfig, Fatal, Local, Platform, Side};
use wlwire::frame::{self, Desc, DescOut, Unit};
use wlwire::policy::{LeaseGate, Policy};
use wlwire::sys;

use crate::hostfd::HandleKind;
use crate::wl::probe::LeaseCache;

/// What the backend knows about a host descriptor.
pub trait HostFds: Send + Sync {
    /// `hostfd` classification: a DRM file must come out as `DrmLease` of
    /// *our* GPU to be carried at all.
    fn classify(&self, fd: BorrowedFd<'_>) -> HandleKind;
}

/// Called while a WL_SEND is processed.
pub trait SendOps {
    /// PRIME-export host GEM `gem` of the file behind backend handle `owner`
    /// (a guest file's render handle). The dma-buf goes to the compositor and
    /// is closed here after sending.
    fn prime_export(&mut self, owner: u32, gem: u32) -> io::Result<OwnedFd>;
    /// The host syncobj behind backend handle `handle`, for the compositor
    /// (explicit sync, once fences are bridged: the global is hidden until
    /// then). Closed here after sending, like an export.
    fn syncobj(&mut self, _handle: u32) -> io::Result<OwnedFd> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// Called while a WL_RECV is built.
pub trait RecvOps {
    /// Put a descriptor the compositor sent into the handle table, for the
    /// guest to adopt; `desc_kind` is `DESC_DRM_FILE` or `DESC_DMABUF`, and
    /// the descriptor must classify as that (a lease of our GPU; a dma-buf).
    /// Returns (handle, `HK_*`).
    fn adopt(&mut self, fd: OwnedFd, desc_kind: u16) -> io::Result<(u32, u32)>;
}

#[derive(Clone)]
pub struct WlConfig {
    /// The host compositor's socket (`--wayland-socket`).
    pub socket: PathBuf,
    /// Offer `wp_drm_lease_device_v1` (only ever for a device whose drm_fd is
    /// a file of our GPU, and only to a guest that can adopt DRM files).
    pub allow_lease: bool,
    /// Bytes waiting for the compositor before WL_SEND says EAGAIN.
    pub max_backlog: usize,
    /// Bytes waiting for the guest before the connection is dropped.
    pub max_queue: usize,
    /// Which lease-device globals are ours, shared by every connection.
    pub lease_cache: Arc<LeaseCache>,
}

impl WlConfig {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            allow_lease: false,
            max_backlog: 4 << 20,
            max_queue: 64 << 20,
            lease_cache: Arc::new(LeaseCache::default()),
        }
    }
}

struct State {
    engine: Engine,
    inbuf: Vec<u8>,
    infds: VecDeque<OwnedFd>,
    to_guest: VecDeque<Unit>,
    to_guest_bytes: usize,
    /// The compositor side is finished (EOF, error, or a fatal protocol
    /// error); HANGUP is queued.
    closed: bool,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    sock: UnixStream,
    /// Readable while `to_guest` is not empty.
    ready: OwnedFd,
    /// Wakes the reader thread (new streams, output waiting for POLLOUT, stop).
    wake: OwnedFd,
    host: Arc<dyn HostFds>,
    cfg: WlConfig,
}

pub struct WlConn {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// Descriptors on the compositor's side of the engine: classification for
/// what the compositor sends, PRIME export for what the guest sends.
struct HostPlat<'a> {
    host: &'a dyn HostFds,
    send: Option<&'a mut dyn SendOps>,
}

impl Platform for HostPlat<'_> {
    fn dmabuf_out(&mut self, fd: OwnedFd) -> DescOut {
        // Export mode: a host client's buffer, for the guest compositor.
        match self.host.classify(fd.as_fd()) {
            HandleKind::Dmabuf => {
                let size = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) }.max(0) as u64;
                DescOut {
                    desc: Desc {
                        c: size,
                        ..Desc::new(frame::DESC_DMABUF)
                    },
                    fd: Some(fd),
                }
            }
            k => {
                log::warn!("wayland: a dma-buf argument was a {k:?}; sending a placeholder");
                DescOut::plain(Desc::invalid(frame::DESC_DMABUF))
            }
        }
    }

    fn dmabuf_in(&mut self, desc: &Desc, _fd: Option<OwnedFd>) -> io::Result<OwnedFd> {
        let ops = self
            .send
            .as_mut()
            .ok_or_else(|| io::Error::other("no export path"))?;
        ops.prime_export(desc.a, desc.b).inspect_err(|e| {
            log::warn!(
                "wayland: PRIME export of gem {} on handle {} failed: {e}",
                desc.b,
                desc.a
            )
        })
    }

    fn drm_file_out(&mut self, fd: OwnedFd) -> DescOut {
        match self.host.classify(fd.as_fd()) {
            k @ HandleKind::DrmLease(_) => DescOut {
                desc: Desc {
                    b: k.wire(),
                    ..Desc::new(frame::DESC_DRM_FILE)
                },
                fd: Some(fd),
            },
            k => {
                // Only files of our own GPU can be driven from the guest; the
                // lease global was meant to be hidden for anything else.
                log::warn!(
                    "wayland: the compositor sent a DRM file that is a {k:?}, not a lease of ours"
                );
                DescOut::plain(Desc::invalid(frame::DESC_DRM_FILE))
            }
        }
    }

    fn drm_file_in(&mut self, _desc: &Desc, _fd: Option<OwnedFd>) -> io::Result<OwnedFd> {
        Err(io::Error::other("the guest does not send DRM files"))
    }

    fn syncobj_in(&mut self, desc: &Desc, _fd: Option<OwnedFd>) -> io::Result<OwnedFd> {
        let ops = self
            .send
            .as_mut()
            .ok_or_else(|| io::Error::other("no export path"))?;
        ops.syncobj(desc.a).inspect_err(|e| {
            log::warn!("wayland: syncobj handle {} for the compositor: {e}", desc.a)
        })
    }
}

fn lock(s: &Shared) -> MutexGuard<'_, State> {
    s.state.lock().unwrap_or_else(|p| p.into_inner())
}

impl WlConn {
    /// Connect to the host compositor for a new guest client. Returns the
    /// connection and a duplicate of its readiness eventfd for the event pump.
    pub fn open(cfg: &WlConfig, host: Arc<dyn HostFds>) -> io::Result<(WlConn, OwnedFd)> {
        let sock = UnixStream::connect(&cfg.socket)?;
        let lease = if cfg.allow_lease {
            let cache = cfg.lease_cache.clone();
            let socket = cfg.socket.clone();
            let h = host.clone();
            LeaseGate::Check(Arc::new(move |name| cache.is_ours(&socket, &*h, name)))
        } else {
            LeaseGate::Deny
        };
        Self::start(
            sock,
            cfg,
            host,
            Local::Server,
            Policy {
                drm_file: false,
                lease,
                fences: false,
            },
        )
    }

    /// A host client connected to the export socket (`--wayland-export`): the
    /// compositor is the guest's, and this side faces a client.
    pub fn from_export(
        sock: UnixStream,
        cfg: &WlConfig,
        host: Arc<dyn HostFds>,
    ) -> io::Result<(WlConn, OwnedFd)> {
        // A guest compositor's leases are not offered to host clients.
        Self::start(sock, cfg, host, Local::Client, Policy::default())
    }

    fn start(
        sock: UnixStream,
        cfg: &WlConfig,
        host: Arc<dyn HostFds>,
        local: Local,
        policy: Policy,
    ) -> io::Result<(WlConn, OwnedFd)> {
        sock.set_nonblocking(true)?;
        let mut engine = Engine::new(EngineConfig {
            side: Side::Host,
            local,
            policy,
            rewrites: None,
            synth_released: false,
        });
        engine.hello(0);
        let ready = sys::eventfd()?;
        let wake = sys::eventfd()?;
        let mut st = State {
            engine,
            inbuf: Vec::new(),
            infds: VecDeque::new(),
            to_guest: VecDeque::new(),
            to_guest_bytes: 0,
            closed: false,
            stop: false,
        };
        let hello = st.engine.take_units();
        st.to_guest_bytes += hello.iter().map(|u| u.bytes()).sum::<usize>();
        st.to_guest.extend(hello);
        sys::eventfd_signal(ready.as_raw_fd());
        let shared = Arc::new(Shared {
            state: Mutex::new(st),
            sock,
            ready,
            wake,
            host,
            cfg: cfg.clone(),
        });
        let ready_dup = shared.ready.try_clone()?;
        let s2 = shared.clone();
        let thread = std::thread::Builder::new()
            .name("nvgpu-wl".into())
            .spawn(move || reader(s2))?;
        Ok((
            WlConn {
                shared,
                thread: Some(thread),
            },
            ready_dup,
        ))
    }

    /// WL_SEND: one frame from the guest. On a protocol violation the
    /// connection is ended (the guest learns why from an ERROR record on its
    /// next WL_RECV) and `EPROTO` returned.
    pub fn send(
        &self,
        frame_bytes: &[u8],
        ops: &mut dyn SendOps,
    ) -> Result<protocol::messages::WlSendResp, i32> {
        let s = &*self.shared;
        let mut st = lock(s);
        if st.closed {
            return Err(libc::EPIPE);
        }
        let backlog = st.engine.local_out().len();
        if backlog > s.cfg.max_backlog {
            return Err(libc::EAGAIN);
        }
        let mut plat = HostPlat {
            host: &*s.host,
            send: Some(ops),
        };
        let r = st.engine.from_channel(frame_bytes, Vec::new(), &mut plat);
        if let Err(f) = r {
            fail(s, &mut st, f);
            return Err(libc::EPROTO);
        }
        collect(s, &mut st);
        if let Err(e) = st.engine.local_out().flush(s.sock.as_raw_fd()) {
            hangup(s, &mut st, e.raw_os_error().unwrap_or(libc::EIO));
        }
        let backlog = st.engine.local_out().len() as u32;
        drop(st);
        // New streams to watch, or output waiting for POLLOUT.
        sys::eventfd_signal(s.wake.as_raw_fd());
        Ok(protocol::messages::WlSendResp {
            accepted: frame_bytes.len() as u32,
            backlog,
        })
    }

    /// WL_RECV: as much as fits in `max_bytes` / `max_desc`, as a frame. An
    /// empty frame when nothing is waiting.
    pub fn recv(
        &self,
        max_bytes: u32,
        max_desc: u32,
        ops: &mut dyn RecvOps,
    ) -> Result<Vec<u8>, i32> {
        if (max_bytes as usize) < frame::MIN_FRAME {
            return Err(libc::EINVAL);
        }
        let s = &*self.shared;
        let mut st = lock(s);
        let (mut f, fds) = frame::pack(
            &mut st.to_guest,
            max_bytes as usize,
            max_desc as usize,
            true,
        );
        st.to_guest_bytes = st.to_guest.iter().map(|u| u.bytes()).sum();
        for (i, fd) in fds.into_iter().enumerate() {
            let Some(fd) = fd else { continue };
            let at = frame::FRAME_HDR_LEN + i * frame::DESC_LEN;
            let mut d = Desc::read(&f[at..at + frame::DESC_LEN]);
            match ops.adopt(fd, d.kind) {
                Ok((handle, hk)) => {
                    d.a = handle;
                    d.b = hk;
                }
                Err(e) => {
                    log::warn!(
                        "wayland: adopting a descriptor of kind {} failed: {e}",
                        d.kind
                    );
                    d.flags |= frame::DESC_F_INVALID;
                }
            }
            let mut b = Vec::with_capacity(frame::DESC_LEN);
            d.write(&mut b);
            f[at..at + frame::DESC_LEN].copy_from_slice(&b);
        }
        if st.to_guest.is_empty() {
            sys::eventfd_clear(s.ready.as_raw_fd());
        }
        Ok(f)
    }

    /// The readiness eventfd (the one `open` returned a duplicate of).
    pub fn ready_fd(&self) -> BorrowedFd<'_> {
        self.shared.ready.as_fd()
    }

    /// Engine counters, for logs and tests.
    pub fn stats(&self) -> wlwire::engine::Stats {
        lock(&self.shared).engine.stats.clone()
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.shared).closed
    }
}

impl Drop for WlConn {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        sys::eventfd_signal(self.shared.wake.as_raw_fd());
        let _ = self.shared.sock.shutdown(std::net::Shutdown::Both);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Move the engine's channel output to the guest queue.
fn collect(s: &Shared, st: &mut State) {
    let units = st.engine.take_units();
    if units.is_empty() {
        return;
    }
    for u in units {
        st.to_guest_bytes += u.bytes();
        st.to_guest.push_back(u);
    }
    sys::eventfd_signal(s.ready.as_raw_fd());
    if st.to_guest_bytes > s.cfg.max_queue && !st.closed {
        log::warn!(
            "wayland: the guest has not read {} bytes; dropping the connection",
            st.to_guest_bytes
        );
        hangup(s, st, libc::ENOBUFS);
    }
}

/// End the connection on a protocol error: the guest is told why.
fn fail(s: &Shared, st: &mut State, f: Fatal) {
    match f.blame {
        Blame::Channel | Blame::Remote => {
            log::warn!("wayland: guest protocol error, closing: {}", f.message)
        }
        Blame::Local => log::warn!("wayland: compositor protocol error, closing: {}", f.message),
    }
    let _ = st.engine.take_units();
    if st.engine.local_is_client() {
        // Export mode: the host client is the one to tell, as libwayland
        // would have.
        st.engine.local_out().push(&f.display_error(), Vec::new());
        let _ = st.engine.local_out().flush(s.sock.as_raw_fd());
    }
    let u = f.record();
    st.to_guest_bytes += u.bytes();
    st.to_guest.push_back(u);
    hangup(s, st, libc::EPROTO);
}

fn hangup(s: &Shared, st: &mut State, errno: i32) {
    if st.closed {
        return;
    }
    st.closed = true;
    let _ = s.sock.shutdown(std::net::Shutdown::Both);
    st.to_guest.push_back(Unit {
        rec: frame::record(frame::REC_HANGUP, 0, errno as u32, &[]),
        descs: Vec::new(),
    });
    sys::eventfd_signal(s.ready.as_raw_fd());
}

fn reader(s: Arc<Shared>) {
    let sock = s.sock.as_raw_fd();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let (want_out, streams, closed) = {
            let st = lock(&s);
            if st.stop {
                return;
            }
            (
                st.engine.local_out_len() > 0,
                st.engine.stream_interest(),
                st.closed,
            )
        };
        let mut pfds: Vec<libc::pollfd> = Vec::with_capacity(2 + streams.len());
        let mut ev = 0;
        if !closed {
            ev |= libc::POLLIN;
            if want_out {
                ev |= libc::POLLOUT;
            }
        }
        pfds.push(libc::pollfd {
            fd: if closed { -1 } else { sock },
            events: ev,
            revents: 0,
        });
        pfds.push(libc::pollfd {
            fd: s.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for i in &streams {
            let mut e = 0;
            if i.read {
                e |= libc::POLLIN;
            }
            if i.write {
                e |= libc::POLLOUT;
            }
            pfds.push(libc::pollfd {
                fd: if e == 0 { -1 } else { i.fd },
                events: e,
                revents: 0,
            });
        }
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 1000) };
        if r < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("wayland: poll: {}", io::Error::last_os_error());
            return;
        }
        if pfds[1].revents != 0 {
            sys::eventfd_clear(s.wake.as_raw_fd());
        }
        let mut st = lock(&s);
        if st.stop {
            return;
        }
        let rev = pfds[0].revents;
        if !st.closed && rev & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            // Read until the socket is empty: never leave the compositor's
            // buffer to fill.
            loop {
                let mut fds = Vec::new();
                match sys::recv_with_fds(sock, &mut buf, &mut fds) {
                    Ok(0) => {
                        hangup(&s, &mut st, 0);
                        break;
                    }
                    Ok(n) => {
                        let State {
                            engine,
                            inbuf,
                            infds,
                            ..
                        } = &mut *st;
                        inbuf.extend_from_slice(&buf[..n]);
                        infds.extend(fds);
                        let mut plat = HostPlat {
                            host: &*s.host,
                            send: None,
                        };
                        if let Err(f) = engine.from_local(inbuf, infds, &mut plat) {
                            fail(&s, &mut st, f);
                            break;
                        }
                        collect(&s, &mut st);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        log::warn!("wayland: reading the compositor: {e}");
                        hangup(&s, &mut st, e.raw_os_error().unwrap_or(libc::EIO));
                        break;
                    }
                }
            }
        }
        if !st.closed && rev & libc::POLLOUT != 0 {
            if let Err(e) = st.engine.local_out().flush(sock) {
                hangup(&s, &mut st, e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
        for (i, p) in streams.iter().zip(&pfds[2..]) {
            if p.revents != 0 {
                let rd = p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
                let wr = p.revents & (libc::POLLOUT | libc::POLLERR) != 0;
                st.engine.stream_io(i.id, rd, wr);
            }
        }
        collect(&s, &mut st);
    }
}

/// Raw descriptor of the compositor socket, for tests.
#[cfg(test)]
pub(crate) fn sock_fd(c: &WlConn) -> std::os::fd::RawFd {
    c.shared.sock.as_raw_fd()
}
