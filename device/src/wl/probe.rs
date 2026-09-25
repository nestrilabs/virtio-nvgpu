//! Which of the compositor's lease devices are for our GPU.
//!
//! `wp_drm_lease_device_v1` is one global per DRM device the compositor
//! drives, and nothing in the registry says which device: that is only known
//! from the `drm_fd` it sends after a bind. A guest can only ever use a lease
//! of the GPU it is proxying (the lease file's KMS ioctls run against our
//! nvidia-drm), so a lease device for any other GPU must be hidden -- and
//! hidden *before* the guest sees the global, not after it bound it.
//!
//! So the first time a connection sees a lease-device global whose name is not
//! yet known, the backend opens a private connection of its own, binds every
//! lease device, classifies each `drm_fd`, and remembers the answer by global
//! name for every later connection (global names are per compositor, not per
//! client). The probe is a few round trips on a socket nobody else sees, bounded
//! by a timeout.
//!
//! **Where it runs.** A probe can take a couple of seconds on a compositor that
//! is busy (a modeset, a stalled output, a login), so it runs where nothing
//! waits on it: in the connection's reader thread, *before* the engine sees the
//! bytes and with no lock held but its own ([`LeaseCache::resolve`]). The
//! registry filter itself only looks answers up ([`LeaseCache::lookup`]) under
//! the connection's lock, which WL_SEND and WL_RECV take from the queue thread
//! with the backend mutex held -- a probe there stalled every request of the VM.
//! One probe runs at a time; a connection that finds one running waits for its
//! answer, holding nothing anyone else needs.
//!
//! **What is remembered.** Only answers: a name whose `drm_fd` arrived and was
//! classified. A probe that failed or timed out, or a device that sent no
//! `drm_fd`, leaves the name unknown -- hidden from the connection that asked,
//! and asked about again by a later one once a backoff has passed (5 s, doubling
//! to a minute), so one slow moment of the compositor does not hide leasing for
//! the life of the backend.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use wlwire::proto::{self, Dir, IfaceId, iface, op};
use wlwire::sys;
use wlwire::wire::{self, MsgBuilder, Val, peek_header};

use crate::hostfd::HandleKind;
use crate::wl::conn::HostFds;

const TIMEOUT: Duration = Duration::from_secs(1);
const BACKOFF_FIRST: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

pub struct LeaseCache {
    /// Answers by global name. Held only to look up or insert.
    known: Mutex<HashMap<u32, bool>>,
    /// Held for the whole of a probe: one at a time. Guards when the next
    /// may start after one that left names unanswered.
    probing: Mutex<Backoff>,
}

struct Backoff {
    not_before: Option<Instant>,
    next: Duration,
}

impl Default for LeaseCache {
    fn default() -> Self {
        Self::with_backoff(BACKOFF_FIRST)
    }
}

impl LeaseCache {
    /// A cache whose first retry after a failed probe waits `first`.
    pub fn with_backoff(first: Duration) -> Self {
        Self {
            known: Mutex::new(HashMap::new()),
            probing: Mutex::new(Backoff {
                not_before: None,
                next: first,
            }),
        }
    }

    /// The answer for lease-device global `name`, if one is known. Never
    /// probes: this is what the registry filter asks, under the connection's
    /// lock. Unknown means hidden, this time.
    pub fn lookup(&self, name: u32) -> Option<bool> {
        let known = self.known.lock().unwrap_or_else(|p| p.into_inner());
        known.get(&name).copied()
    }

    /// Make the answers for `names` known, probing the compositor if any is
    /// not and no backoff is running. Call it with no other lock held: it
    /// may take a couple of seconds.
    pub fn resolve(&self, socket: &Path, host: &dyn HostFds, names: &[u32]) {
        let missing = |c: &Self| names.iter().any(|n| c.lookup(*n).is_none());
        if !missing(self) {
            return;
        }
        let mut b = self.probing.lock().unwrap_or_else(|p| p.into_inner());
        // Another connection's probe may have answered while we waited.
        if !missing(self) {
            return;
        }
        if b.not_before.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let r = probe(socket, host);
        let failed = match r {
            Ok(found) => {
                let mut known = self.known.lock().unwrap_or_else(|p| p.into_inner());
                known.extend(found);
                drop(known);
                if missing(self) {
                    log::info!(
                        "wayland: a lease device sent no drm_fd; asking again in {:?}",
                        b.next
                    );
                    true
                } else {
                    false
                }
            }
            Err(e) => {
                log::warn!(
                    "wayland: probing the compositor's lease devices failed ({e}); \
                     hidden for now, asking again in {:?}",
                    b.next
                );
                true
            }
        };
        if failed {
            b.not_before = Some(Instant::now() + b.next);
            b.next = (b.next * 2).min(BACKOFF_MAX);
        } else {
            b.not_before = None;
            b.next = BACKOFF_FIRST;
        }
    }

    /// Whether lease-device global `name` hands out files of our GPU:
    /// [`resolve`](Self::resolve), then [`lookup`](Self::lookup).
    pub fn is_ours(&self, socket: &Path, host: &dyn HostFds, name: u32) -> bool {
        self.resolve(socket, host, &[name]);
        self.lookup(name).unwrap_or(false)
    }
}

/// The names of the `wp_drm_lease_device_v1` globals announced among the
/// whole messages at the front of `buf` (bytes from the compositor not yet
/// run through the engine), for [`LeaseCache::resolve`] to answer before the
/// registry filter asks.
///
/// Read by shape, not by object: any event with opcode 0 (`wl_registry.global`)
/// whose arguments parse as (uint, that string, uint). Which objects are
/// registries is the engine's to know, under the lock this runs outside of;
/// a message of another object that happens to match costs only a probe,
/// and the filter still asks by the registry's own global.
pub fn lease_globals(mut buf: &[u8]) -> Vec<u32> {
    const NAME: &[u8] = b"wp_drm_lease_device_v1\0";
    let mut out = Vec::new();
    while let Some(h) = peek_header(buf) {
        let size = h.size as usize;
        if size < 8 || size > buf.len() {
            break;
        }
        let m = &buf[..size];
        buf = &buf[size..];
        if h.opcode != op::wl_registry::EVT_GLOBAL || size < 16 {
            continue;
        }
        let word = |at: usize| u32::from_ne_bytes(m[at..at + 4].try_into().unwrap());
        let len = word(12) as usize;
        if len == NAME.len() && m.get(16..16 + len) == Some(NAME) {
            out.push(word(8));
        }
    }
    out
}

/// A minimal client: enough object tracking to parse the replies, and to
/// count descriptors.
struct Probe {
    sock: UnixStream,
    objects: HashMap<u32, IfaceId>,
    buf: Vec<u8>,
    fds: VecDeque<OwnedFd>,
}

impl Probe {
    fn send(&mut self, m: Vec<u8>) -> io::Result<()> {
        let mut off = 0;
        let deadline = Instant::now() + TIMEOUT;
        while off < m.len() {
            match sys::send_with_fds(self.sock.as_raw_fd(), &m[off..], &[]) {
                Ok(n) => off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Read events until `callback.done`, handing each to `f` with the
    /// descriptors it carries.
    fn until_done(
        &mut self,
        callback: u32,
        f: &mut dyn FnMut(u32, u16, &[wire::At<'_>], Vec<OwnedFd>),
    ) -> io::Result<()> {
        let deadline = Instant::now() + TIMEOUT;
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            while let Some(h) = peek_header(&self.buf) {
                let size = h.size as usize;
                if size < 8 || size > wire::MAX_MSG {
                    return Err(io::Error::other("bad message from the compositor"));
                }
                if self.buf.len() < size {
                    break;
                }
                let msg: Vec<u8> = self.buf.drain(..size).collect();
                let Some(&ifc) = self.objects.get(&h.object) else {
                    return Err(io::Error::other("event for an unknown object"));
                };
                let Some(desc) = iface(ifc).messages(Dir::Event).get(h.opcode as usize) else {
                    return Err(io::Error::other("unknown event"));
                };
                let args =
                    wire::parse(desc, &msg).map_err(|e| io::Error::other(format!("{e:?}")))?;
                let mut fds = Vec::new();
                for _ in 0..desc.nfds {
                    fds.push(
                        self.fds
                            .pop_front()
                            .ok_or_else(|| io::Error::other("descriptor expected"))?,
                    );
                }
                for (a, at) in desc.args.iter().zip(&args) {
                    if let (Val::NewId { id, .. }, Some(i)) = (at.val, a.iface) {
                        self.objects.insert(id, i);
                    }
                }
                if h.object == callback && ifc == proto::WL_CALLBACK {
                    return Ok(());
                }
                f(h.object, h.opcode, &args, fds);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut p = libc::pollfd {
                fd: self.sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut p, 1, left.as_millis() as i32) };
            let mut got = Vec::new();
            match sys::recv_with_fds(self.sock.as_raw_fd(), &mut chunk, &mut got) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    self.fds.extend(got);
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// Bind every lease device on a private connection and classify its drm_fd.
/// Returns (global name, is ours) for each whose drm_fd arrived.
pub fn probe(socket: &Path, host: &dyn HostFds) -> io::Result<Vec<(u32, bool)>> {
    let sock = UnixStream::connect(socket)?;
    sock.set_nonblocking(true)?;
    let mut p = Probe {
        sock,
        objects: HashMap::from([(1, proto::WL_DISPLAY)]),
        buf: Vec::new(),
        fds: VecDeque::new(),
    };
    p.objects.insert(2, proto::WL_REGISTRY);
    p.objects.insert(3, proto::WL_CALLBACK);
    p.send(
        MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish(),
    )?;
    p.send(
        MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(3)
            .finish(),
    )?;
    let mut devices = Vec::new();
    p.until_done(3, &mut |obj, opc, args, _| {
        if obj == 2 && opc == op::wl_registry::EVT_GLOBAL {
            if let (Val::Uint(name), Val::Str(Some(b"wp_drm_lease_device_v1"))) =
                (args[0].val, args[1].val)
            {
                devices.push(name);
            }
        }
    })?;
    if devices.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_obj = HashMap::new();
    for (i, name) in devices.iter().enumerate() {
        let id = 10 + i as u32;
        p.objects.insert(id, proto::WP_DRM_LEASE_DEVICE_V1);
        by_obj.insert(id, *name);
        p.send(
            MsgBuilder::new(2, op::wl_registry::REQ_BIND)
                .uint(*name)
                .generic_new_id("wp_drm_lease_device_v1", 1, id)
                .finish(),
        )?;
    }
    p.objects.insert(4, proto::WL_CALLBACK);
    p.send(
        MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(4)
            .finish(),
    )?;
    // Only what a drm_fd answered: a device that sent none is not known to
    // be anyone's, and is asked about again later rather than hidden for good.
    let mut result: HashMap<u32, bool> = HashMap::new();
    p.until_done(4, &mut |obj, opc, _, fds| {
        if opc == op::wp_drm_lease_device_v1::EVT_DRM_FD {
            if let (Some(name), Some(fd)) = (by_obj.get(&obj), fds.first()) {
                let ours = matches!(host.classify(fd.as_fd()), HandleKind::DrmLease(_));
                result.insert(*name, ours);
            }
        }
    })?;
    Ok(result.into_iter().collect())
}
