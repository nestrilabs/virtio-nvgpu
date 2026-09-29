// SPDX-License-Identifier: Apache-2.0
//! Descriptors this process was handed across exec rather than made: the
//! listening sockets of systemd's socket activation (sd_listen_fds(3):
//! `LISTEN_PID`, `LISTEN_FDS`, `LISTEN_FDNAMES`, numbered from 3 up) and
//! `--socket-fd N`. Whoever started the backend bound them -- root, or
//! systemd -- so the backend needs no writable directory for them, and its
//! user never owns the directory their paths are in (SECURITY.md §22).

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

/// Take ownership of the inherited descriptor `fd`, and make it
/// close-on-exec and blocking (both listeners' accept loops block). EBADF
/// when nothing is open there, and for 0-2 (stdio is never a listening
/// socket).
///
/// The caller's side of the bargain, which this cannot check: `fd` came
/// across exec, and nothing else in this process has taken it as its own.
/// The backend calls this at start, once per number, before it opens
/// anything; every number it claims was open when it started, so no
/// descriptor of its own can have been given that number.
pub fn claim(fd: RawFd) -> io::Result<OwnedFd> {
    if fd < 3 {
        return Err(io::Error::from_raw_os_error(libc::EBADF));
    }
    // SAFETY: integer arguments.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: integer arguments.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: integer arguments.
    let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: integer arguments.
    if status & libc::O_NONBLOCK != 0
        && unsafe { libc::fcntl(fd, libc::F_SETFL, status & !libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is open (F_GETFD above), and by this function's contract
    // it was inherited and is claimed once, by this call: it has no other
    // owner in this process.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// What a socket is: its domain, type and whether it listens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketKind {
    pub domain: i32,
    pub ty: i32,
    pub listening: bool,
}

fn sockopt_int(fd: BorrowedFd<'_>, opt: libc::c_int) -> io::Result<libc::c_int> {
    let mut v: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `v` and `len` are live locals of the sizes given; the kernel
    // writes at most `len` bytes into `v`.
    let r = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            opt,
            (&mut v as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(v)
}

/// `SO_DOMAIN`, `SO_TYPE` and `SO_ACCEPTCONN` of `fd`: ENOTSOCK for a
/// descriptor that is not a socket.
pub fn socket_kind(fd: BorrowedFd<'_>) -> io::Result<SocketKind> {
    Ok(SocketKind {
        domain: sockopt_int(fd, libc::SO_DOMAIN)?,
        ty: sockopt_int(fd, libc::SO_TYPE)?,
        listening: sockopt_int(fd, libc::SO_ACCEPTCONN)? != 0,
    })
}

/// How many threads this process has, from `/proc/self/task`.
fn threads() -> Option<usize> {
    std::fs::read_dir("/proc/self/task").ok().map(|d| d.count())
}

/// Remove `names` from the environment, as sd_listen_fds(3) does with
/// `unset_environment`: they describe descriptors this process has now
/// claimed, and mean nothing to anything it might start. Only while the
/// process has one thread, where nothing can be reading the environment at
/// the same time; with more (or no /proc) it leaves them, and says so with
/// `false`.
pub fn unset_env(names: &[&str]) -> bool {
    if threads() != Some(1) {
        return false;
    }
    for n in names {
        // SAFETY: remove_var races only with another thread reading or
        // writing the environment, and this process has one thread.
        unsafe { std::env::remove_var(n) };
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsFd, IntoRawFd};

    #[test]
    fn a_listening_unix_socket_is_claimed_and_known_for_what_it_is() {
        let dir = std::env::temp_dir().join(format!("nvgpu-inherit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let l = std::os::unix::net::UnixListener::bind(dir.join("s")).unwrap();
        let raw = l.into_raw_fd();
        let fd = claim(raw).unwrap();
        assert_eq!(fd.as_raw_fd(), raw);
        let k = socket_kind(fd.as_fd()).unwrap();
        assert_eq!(
            k,
            SocketKind {
                domain: libc::AF_UNIX,
                ty: libc::SOCK_STREAM,
                listening: true
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stdio_and_closed_numbers_are_not_claimed() {
        for fd in [-1, 0, 1, 2] {
            assert_eq!(claim(fd).unwrap_err().raw_os_error(), Some(libc::EBADF));
        }
        // A number far above anything this test process has open.
        assert_eq!(
            claim(1 << 20).unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
    }

    #[test]
    fn a_pipe_is_not_a_socket() {
        let (r, _w) = crate::sys::fd::pipe2(libc::O_CLOEXEC).unwrap();
        assert_eq!(
            socket_kind(r.as_fd()).unwrap_err().raw_os_error(),
            Some(libc::ENOTSOCK)
        );
    }
}
