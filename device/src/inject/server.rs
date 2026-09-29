// SPDX-License-Identifier: Apache-2.0
//! The helper's socket: who may connect, and every framing rule of its
//! packets ([`serve_packet`], which the fuzz target drives).

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use protocol::inject::{
    INJ_MAX_PACKET, INJ_OP_HELLO, INJ_OP_IMPORT, INJ_OP_IMPORT_SYNCOBJ, INJ_OP_RELEASE,
    INJ_VERSION, InjMalformed, InjReply, InjRequest, parse_request,
};

use super::MAX_PEERS;
use super::registry::Registry;
use crate::privfd::PrivateFd;

struct Shared {
    registry: Arc<Registry>,
    uid: u32,
    /// The VMM's uid, once known (u32::MAX until then): never a helper's,
    /// even the one `uid` names (`InjectServer::refuse_uid`).
    vmm_uid: std::sync::atomic::AtomicU32,
    stop: AtomicBool,
    peers: AtomicUsize,
    next_peer: AtomicU64,
    /// Every live connection, to shut down with the server.
    conns: Mutex<HashMap<u64, Arc<PrivateFd>>>,
}

/// The listener and its threads.
pub struct InjectServer {
    path: PathBuf,
    shared: Arc<Shared>,
    listener: Arc<OwnedFd>,
    idle: Mutex<bool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl InjectServer {
    /// Bind at `path` (replacing a stale socket of ours there) for peers of
    /// uid `uid`, without the accepting thread: the backend binds before its
    /// sandbox, which leaves it no directory to make a socket in, and starts
    /// threads after ([`InjectServer::start`]).
    pub fn bind_idle(path: &Path, uid: u32, registry: Arc<Registry>) -> io::Result<Self> {
        // As the export socket is (sockpath.rs): only a stale socket of ours
        // is replaced, and nobody can connect before it is 0600.
        crate::sockpath::check_replaceable(path, "inject")?;
        let l = crate::sockpath::bind_private(path, |p| {
            crate::sys::net::seqpacket_listen(p, MAX_PEERS as i32)
        })?;
        crate::privfd::register(l.as_raw_fd());
        Ok(Self {
            path: path.to_path_buf(),
            shared: Arc::new(Shared {
                registry,
                uid,
                vmm_uid: std::sync::atomic::AtomicU32::new(u32::MAX),
                stop: AtomicBool::new(false),
                peers: AtomicUsize::new(0),
                next_peer: AtomicU64::new(1),
                conns: Mutex::new(HashMap::new()),
            }),
            listener: Arc::new(l),
            idle: Mutex::new(true),
            thread: Mutex::new(None),
        })
    }

    /// Refuse peers of `uid` -- the VMM's, as the transport learns it --
    /// whatever `--inject-uid` says: a capture helper is a user of its own,
    /// one per VM, and never the VMM (defence in depth for that rule).
    pub fn refuse_uid(&self, uid: u32) {
        self.shared.vmm_uid.store(uid, Ordering::Release);
    }

    /// Start accepting. Once.
    pub fn start(&self) -> io::Result<()> {
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        if !*idle {
            return Ok(());
        }
        let (s, l) = (self.shared.clone(), self.listener.clone());
        let t = std::thread::Builder::new()
            .name("nvgpu-inject".into())
            .spawn(move || accept_loop(s, l))?;
        *self.thread.lock().unwrap_or_else(|p| p.into_inner()) = Some(t);
        *idle = false;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.shared.registry
    }

    /// Stop accepting, hang up every peer (releasing what each imported),
    /// and wait for the threads. The socket file stays: the sandbox leaves
    /// the backend no unlink, and the next start replaces it.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = crate::sys::net::shutdown(self.listener.as_raw_fd());
        for c in self
            .shared
            .conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            let _ = crate::sys::net::shutdown(c.as_raw_fd());
        }
        if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = t.join();
        }
        // Peer threads end on their shutdown; wait for them to have
        // released what they held.
        for _ in 0..200 {
            if self.shared.peers.load(Ordering::Acquire) == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

impl Drop for InjectServer {
    fn drop(&mut self) {
        crate::privfd::unregister(self.listener.as_raw_fd());
    }
}

impl InjectServer {
    /// [`InjectServer::bind_idle`] for a socket someone else bound and
    /// handed over already listening -- systemd's socket activation, whose
    /// unit sets its path, owner, group and mode (contrib/systemd). `name`
    /// is only for the log. The caller has checked `listener` is a
    /// listening `AF_UNIX` `SOCK_SEQPACKET` socket.
    pub fn from_listener(
        listener: OwnedFd,
        name: PathBuf,
        uid: u32,
        registry: Arc<Registry>,
    ) -> Self {
        crate::privfd::register(listener.as_raw_fd());
        Self {
            path: name,
            shared: Arc::new(Shared {
                registry,
                uid,
                vmm_uid: std::sync::atomic::AtomicU32::new(u32::MAX),
                stop: AtomicBool::new(false),
                peers: AtomicUsize::new(0),
                next_peer: AtomicU64::new(1),
                conns: Mutex::new(HashMap::new()),
            }),
            listener: Arc::new(listener),
            idle: Mutex::new(true),
            thread: Mutex::new(None),
        }
    }
}

fn accept_loop(s: Arc<Shared>, l: Arc<OwnedFd>) {
    loop {
        let conn = crate::sys::net::accept(l.as_raw_fd());
        if s.stop.load(Ordering::Relaxed) {
            return;
        }
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                if let Some(d) = crate::sockpath::accept_backoff(&e) {
                    log::warn!("inject: accept: {e}");
                    std::thread::sleep(d);
                } else if !matches!(e.raw_os_error(), Some(libc::EINTR | libc::ECONNABORTED)) {
                    // The listener was shut down, or is gone.
                    return;
                }
                continue;
            }
        };
        match crate::sys::fd::peer_cred(conn.as_raw_fd()) {
            Ok(c) if c.uid == s.vmm_uid.load(Ordering::Acquire) => {
                log::warn!(
                    "inject: refusing a connection from uid {} (pid {}): that is the VMM's uid",
                    c.uid,
                    c.pid
                );
                continue;
            }
            Ok(c) if c.uid == s.uid => {}
            Ok(c) => {
                log::warn!(
                    "inject: refusing a connection from uid {} (pid {}); only uid {} may inject",
                    c.uid,
                    c.pid,
                    s.uid
                );
                continue;
            }
            Err(e) => {
                log::warn!("inject: SO_PEERCRED: {e}");
                continue;
            }
        }
        if s.peers.fetch_add(1, Ordering::AcqRel) >= MAX_PEERS {
            s.peers.fetch_sub(1, Ordering::AcqRel);
            log::warn!("inject: {MAX_PEERS} helpers connected already; refusing another");
            continue;
        }
        let peer = s.next_peer.fetch_add(1, Ordering::Relaxed);
        let conn = Arc::new(PrivateFd::new(conn));
        s.conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(peer, conn.clone());
        let s2 = s.clone();
        let spawned = std::thread::Builder::new()
            .name("nvgpu-inject-peer".into())
            .spawn(move || {
                serve_peer(&s2, peer, &conn);
                s2.conns
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&peer);
                let n = s2.registry.release_peer(peer);
                if n > 0 {
                    log::info!("inject: a helper hung up; its {n} buffer(s) released");
                }
                s2.peers.fetch_sub(1, Ordering::AcqRel);
            });
        if let Err(e) = spawned {
            log::warn!("inject: no thread for a helper: {e}");
            s.conns
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&peer);
            s.peers.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn reply(conn: &PrivateFd, r: &InjReply) -> bool {
    crate::sys::net::send_packet(conn.as_raw_fd(), &r.to_bytes(), &[]).is_ok()
}

/// One helper connection, until it hangs up or breaks the protocol.
fn serve_peer(s: &Shared, peer: u64, conn: &PrivateFd) {
    let mut hello = false;
    let mut buf = [0u8; INJ_MAX_PACKET];
    loop {
        let p = match crate::sys::net::recv_packet(conn.as_raw_fd(), &mut buf) {
            Ok(p) => p,
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(_) => return,
        };
        // The helper's descriptors are the backend's own from here: no
        // IOCTL2 may adopt one of their numbers (privfd.rs).
        let fds: Vec<PrivateFd> = p.fds.into_iter().map(PrivateFd::new).collect();
        if p.len == 0 && fds.is_empty() && !p.truncated {
            return; // hangup
        }
        let r = serve_packet(
            &s.registry,
            peer,
            &mut hello,
            &buf[..p.len],
            p.truncated || p.fds_truncated,
            fds,
        );
        let (r, end) = match r {
            Served::Reply(r) => (r, false),
            Served::Last(r) => (r, true),
            Served::Hangup => return,
        };
        if !reply(conn, &r) || end {
            return;
        }
    }
}

/// What one packet gets.
#[derive(Debug, PartialEq, Eq)]
pub enum Served {
    Reply(InjReply),
    /// A reply, and then the connection ends.
    Last(InjReply),
    /// The connection ends with no reply.
    Hangup,
}

/// One packet of helper connection `peer` (whose HELLO state is `hello`):
/// `bytes` as received, `truncated` if the kernel cut the packet or its
/// descriptors. Every framing rule is here, apart from the socket, for the
/// fuzzer to drive (fuzzing/inject.rs).
pub fn serve_packet(
    reg: &Registry,
    peer: u64,
    hello: &mut bool,
    bytes: &[u8],
    truncated: bool,
    fds: Vec<PrivateFd>,
) -> Served {
    if truncated {
        log::warn!("inject: a helper sent an oversized packet; disconnected");
        return Served::Hangup;
    }
    let req = match parse_request(bytes) {
        Ok(r) => r,
        Err(m) => {
            let why = match m {
                InjMalformed::Size => "a packet of the wrong size",
                InjMalformed::Op => "an unknown op",
                InjMalformed::Reserved => "a reserved field set",
            };
            log::warn!("inject: a helper sent {why}; disconnected");
            return Served::Hangup;
        }
    };
    let (op, takes_fds) = match req {
        InjRequest::Hello(_) => (INJ_OP_HELLO, false),
        InjRequest::Import(_) => (INJ_OP_IMPORT, true),
        InjRequest::Release(_) => (INJ_OP_RELEASE, false),
        InjRequest::ImportSyncobj(_) => (INJ_OP_IMPORT_SYNCOBJ, true),
    };
    if !takes_fds && !fds.is_empty() {
        log::warn!("inject: descriptors on a message that takes none; disconnected");
        return Served::Hangup;
    }
    if !*hello && op != INJ_OP_HELLO {
        log::warn!("inject: a request before HELLO; disconnected");
        return Served::Hangup;
    }
    let mut r = InjReply {
        op,
        ..InjReply::default()
    };
    match req {
        InjRequest::Hello(h) => {
            if *hello || h.version != INJ_VERSION || h.flags != 0 {
                r.status = -libc::EPROTO;
                r.version = INJ_VERSION;
                log::warn!(
                    "inject: HELLO for version {} (flags {:#x}); this backend speaks {INJ_VERSION}",
                    h.version,
                    h.flags
                );
                return Served::Last(r);
            }
            *hello = true;
            let (max_buffers, max_bytes, max_syncobjs) = reg.limits();
            r.version = INJ_VERSION;
            r.max_buffers = max_buffers as u32;
            r.max_bytes = max_bytes;
            r.max_syncobjs = max_syncobjs as u32;
        }
        InjRequest::Import(imp) => match reg.import(peer, &imp, fds) {
            Ok((id, token)) => {
                r.id = id;
                r.token = token;
            }
            Err(e) => r.status = -e,
        },
        InjRequest::ImportSyncobj(so) => match reg.import_syncobj(peer, so.flags, fds) {
            Ok((id, token)) => {
                r.id = id;
                r.token = token;
            }
            Err(e) => r.status = -e,
        },
        InjRequest::Release(rel) => {
            if let Err(e) = reg.release(peer, rel.id) {
                r.status = -e;
            }
        }
    }
    Served::Reply(r)
}
