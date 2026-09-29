// SPDX-License-Identifier: Apache-2.0
//! The sockets the backend binds at a path of the operator's choosing: the
//! Wayland export socket (`wl/export.rs`) and the capture helper's
//! (`inject/server.rs`).
//!
//! Each is bound so that nobody can connect in the moment before its mode is
//! set: in a new directory only we can enter, made 0600 there, and renamed
//! into place ([`bind_private`]). Not under a temporary umask: that is
//! process-wide, and would race every other thread creating files. What is
//! at the path already is replaced only if it is a socket of ours, one a
//! backend before this one left ([`check_replaceable`]): never a file, and
//! never another user's socket in a shared directory. And an accept loop on
//! one rests when it is out of descriptors rather than spinning on the
//! connection it cannot take ([`accept_backoff`]).

#![forbid(unsafe_code)]

use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

/// Whether what is at `path` may be replaced by a socket of ours (nothing,
/// or a socket we own). `what` names the socket in the error.
pub fn check_replaceable(path: &Path, what: &str) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => may_replace(
            m.file_type().is_socket(),
            m.uid(),
            crate::sys::proc::uid(),
            what,
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// The rule [`check_replaceable`] applies to what it finds.
fn may_replace(is_socket: bool, owner: u32, me: u32, what: &str) -> io::Result<()> {
    if !is_socket {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{what} path exists and is not a socket"),
        ));
    }
    if owner != me {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{what} path is a socket of uid {owner}, not ours"),
        ));
    }
    Ok(())
}

/// Bind at `path` with no moment in which anyone else may connect: `listen`
/// binds a listening socket at the path it is given, inside a new directory
/// only we can enter; that socket is made 0600 there and renamed into place
/// (which also replaces a stale socket of ours at once).
pub fn bind_private<T>(path: &Path, listen: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
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
        let l = listen(&inner)?;
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
/// fails at once, so without a pause the thread spins on it. `None` for any
/// other error.
pub fn accept_backoff(e: &io::Error) -> Option<Duration> {
    match e.raw_os_error() {
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => {
            Some(Duration::from_millis(100))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stale socket is replaced only if it is ours; anything else at the
    /// path is refused.
    #[test]
    fn only_a_socket_of_our_own_is_replaced() {
        assert!(may_replace(true, 1000, 1000, "test").is_ok());
        assert_eq!(
            may_replace(true, 1001, 1000, "test").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            may_replace(false, 1000, 1000, "test").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    /// Out of descriptors, an accept loop waits before trying again rather
    /// than spinning on the connection it cannot take.
    #[test]
    fn accept_backs_off_when_out_of_descriptors() {
        for e in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert!(accept_backoff(&io::Error::from_raw_os_error(e)).is_some());
        }
        assert!(accept_backoff(&io::Error::from_raw_os_error(libc::ECONNABORTED)).is_none());
    }
}
