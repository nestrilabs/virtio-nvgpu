//! Export mode (`--wayland-export PATH`): host clients reach a compositor
//! running in the guest.
//!
//! The backend listens on a socket of its own; each connection a host client
//! makes becomes a channel toward the guest, where the daemon in `--export`
//! mode connects it to the guest compositor. The socket is created mode 0600
//! and every peer's uid is checked against ours (`SO_PEERCRED`): the guest
//! compositor is as trusted as the VM, which is to say not at all, and only
//! this user's own programs are meant to be its clients.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use wlwire::sys;

/// Connections waiting for the guest to take them; more are refused.
const MAX_PENDING: usize = 16;

pub struct WlExport {
    path: PathBuf,
    pending: Mutex<VecDeque<UnixStream>>,
    /// Readable while connections are pending.
    ready: OwnedFd,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Bound but not yet accepting ([`WlExport::bind_idle`]).
    idle: Mutex<Option<UnixListener>>,
}

fn peer_uid(s: &UnixStream) -> io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

impl WlExport {
    /// Listen on `path` (replacing a stale socket there). Returns the listener
    /// and a duplicate of its readiness eventfd.
    pub fn bind(path: &Path) -> io::Result<(Arc<WlExport>, OwnedFd)> {
        let (e, ready) = Self::bind_idle(path)?;
        e.start()?;
        Ok((e, ready))
    }

    /// `bind` without the accepting thread, which [`WlExport::start`] starts:
    /// the backend binds before its sandbox, which leaves it no directory to
    /// create a socket in, and starts threads after, since a user namespace
    /// is entered only by a process with one (device::sandbox).
    pub fn bind_idle(path: &Path) -> io::Result<(Arc<WlExport>, OwnedFd)> {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_socket_like() => std::fs::remove_file(path)?,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "export path exists and is not a socket",
                ));
            }
            Err(_) => {}
        }
        // Not under a temporary umask: that is process-wide, and would race
        // every other thread creating files. Whoever connects in the moment
        // before the chmod still meets the uid check below.
        let l = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let ready = sys::eventfd()?;
        let ready_dup = ready.try_clone()?;
        let e = Arc::new(WlExport {
            path: path.to_path_buf(),
            pending: Mutex::new(VecDeque::new()),
            ready,
            stop: AtomicBool::new(false),
            thread: Mutex::new(None),
            idle: Mutex::new(None),
        });
        *e.idle.lock().unwrap() = Some(l);
        Ok((e, ready_dup))
    }

    /// Start accepting on a listener [`WlExport::bind_idle`] bound. Once.
    pub fn start(self: &Arc<Self>) -> io::Result<()> {
        let Some(l) = self.idle.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return Ok(());
        };
        let e2 = self.clone();
        let t = std::thread::Builder::new()
            .name("nvgpu-wl-export".into())
            .spawn(move || accept_loop(e2, l))?;
        *self.thread.lock().unwrap_or_else(|p| p.into_inner()) = Some(t);
        Ok(())
    }

    /// The next host connection, if any (OPEN(DEV_WAYLAND, WL_OPEN_ACCEPT)).
    pub fn accept_pending(&self) -> Option<UnixStream> {
        let mut q = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let s = q.pop_front();
        if q.is_empty() {
            sys::eventfd_clear(self.ready.as_raw_fd());
        }
        s
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop listening and remove the socket.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept() with a connection of our own.
        let _ = UnixStream::connect(&self.path);
        if let Some(t) = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

trait SocketLike {
    fn is_socket_like(&self) -> bool;
}
impl SocketLike for std::fs::FileType {
    fn is_socket_like(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        self.is_socket()
    }
}

fn accept_loop(e: Arc<WlExport>, l: UnixListener) {
    let me = unsafe { libc::getuid() };
    for s in l.incoming() {
        if e.stop.load(Ordering::Relaxed) {
            return;
        }
        let s = match s {
            Ok(s) => s,
            Err(err) => {
                log::warn!("wayland export: accept: {err}");
                continue;
            }
        };
        match peer_uid(&s) {
            Ok(uid) if uid == me => {}
            Ok(uid) => {
                log::warn!("wayland export: refusing a connection from uid {uid}");
                continue;
            }
            Err(err) => {
                log::warn!("wayland export: SO_PEERCRED: {err}");
                continue;
            }
        }
        let mut q = e.pending.lock().unwrap_or_else(|p| p.into_inner());
        if q.len() >= MAX_PENDING {
            log::warn!("wayland export: the guest is not taking connections; refusing one");
            continue;
        }
        q.push_back(s);
        sys::eventfd_signal(e.ready.as_raw_fd());
    }
}
