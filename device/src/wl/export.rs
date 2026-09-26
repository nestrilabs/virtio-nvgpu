//! Export mode (`--wayland-export PATH`): host clients reach a compositor
//! running in the guest.
//!
//! The backend listens on a socket of its own; each connection a host client
//! makes becomes a channel toward the guest, where the daemon in `--export`
//! mode connects it to the guest compositor. The socket is bound in a
//! directory of its own, mode 0700, made mode 0600 there and only then
//! renamed into place, so nobody else can connect in the moment before the
//! chmod; and every peer's uid is checked against ours (`SO_PEERCRED`): the
//! guest compositor is as trusted as the VM, which is to say not at all, and
//! only this user's own programs are meant to be its clients. A stale socket
//! at the path is replaced only if it is ours.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

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
    Ok(crate::sys::fd::peer_cred(s.as_raw_fd())?.uid)
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
        let me = crate::sys::proc::uid();
        match std::fs::symlink_metadata(path) {
            Ok(m) => may_replace(m.file_type().is_socket(), m.uid(), me)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let l = bind_private(path)?;
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

/// Whether what is at the export path may be replaced: a socket of ours (a
/// backend before this one left it), and nothing else -- not a file, and not
/// another user's socket in a shared directory.
fn may_replace(is_socket: bool, owner: u32, me: u32) -> io::Result<()> {
    if !is_socket {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "export path exists and is not a socket",
        ));
    }
    if owner != me {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("export path is a socket of uid {owner}, not ours"),
        ));
    }
    Ok(())
}

/// Bind at `path` with no moment in which anyone else may connect: bound in
/// a new directory only we can enter, made 0600 there, and renamed into
/// place (which also replaces a stale socket of ours at once). Not under a
/// temporary umask: that is process-wide, and would race every other thread
/// creating files.
fn bind_private(path: &Path) -> io::Result<UnixListener> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy()
        .into_owned();
    let mut n = 0;
    let dir = loop {
        let d = parent.join(format!(".{name}.{}.{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&d) {
            Ok(()) => break d,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 16 => n += 1,
            Err(e) => return Err(e),
        }
    };
    let inner = dir.join("s");
    let r = (|| {
        let l = UnixListener::bind(&inner)?;
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&inner, path)?;
        Ok(l)
    })();
    let _ = std::fs::remove_file(&inner);
    let _ = std::fs::remove_dir(&dir);
    r
}

/// How long to wait before the next accept after `e`: out of descriptors or
/// memory, the pending connection stays in the backlog and every accept
/// fails at once, so without a pause the thread spins on it.
fn accept_backoff(e: &io::Error) -> Option<Duration> {
    match e.raw_os_error() {
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => {
            Some(Duration::from_millis(100))
        }
        _ => None,
    }
}

fn accept_loop(e: Arc<WlExport>, l: UnixListener) {
    let me = crate::sys::proc::uid();
    for s in l.incoming() {
        if e.stop.load(Ordering::Relaxed) {
            return;
        }
        let s = match s {
            Ok(s) => s,
            Err(err) => {
                log::warn!("wayland export: accept: {err}");
                if let Some(d) = accept_backoff(&err) {
                    std::thread::sleep(d);
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A stale socket is replaced only if it is ours; anything else at the
    /// path is refused. Before, any socket there was removed.
    #[test]
    fn only_a_socket_of_our_own_is_replaced() {
        assert!(may_replace(true, 1000, 1000).is_ok());
        assert_eq!(
            may_replace(true, 1001, 1000).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            may_replace(false, 1000, 1000).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    /// Bound privately and renamed into place, over a stale socket of ours,
    /// with nothing left beside it.
    #[test]
    fn the_socket_is_bound_in_a_private_directory_and_renamed_into_place() {
        let dir = crate::wl::tests::tmpdir("export-private");
        let path = dir.join("export-0");
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);
        let (x, _ready) = WlExport::bind(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("export-0")]);
        let _c = UnixStream::connect(&path).unwrap();
        x.shutdown();
        // A file that is not a socket is never replaced.
        std::fs::write(&path, b"x").unwrap();
        assert!(WlExport::bind_idle(&path).is_err());
    }

    /// Out of descriptors, the accept loop waits before trying again rather
    /// than spinning on the connection it cannot take.
    #[test]
    fn accept_backs_off_when_out_of_descriptors() {
        for e in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert!(accept_backoff(&io::Error::from_raw_os_error(e)).is_some());
        }
        assert!(accept_backoff(&io::Error::from_raw_os_error(libc::ECONNABORTED)).is_none());
    }
}
