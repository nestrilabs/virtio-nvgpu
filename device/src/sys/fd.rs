//! Descriptors: every system call on one that is not an ioctl.
//!
//! Each function returns what it makes as an `OwnedFd` (so it is closed
//! exactly once), takes a descriptor by `AsFd` where it can, and by `RawFd`
//! where the crate keeps numbers (the pump's watch list, the handle table's
//! lookups). A raw number that is not open is EBADF; one that is open but
//! not meant is still only a read, a write or a flag on one of this
//! process's own files -- never a memory access outside the buffers passed
//! -- which is why those take no `unsafe` of their callers.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn cvt_s(r: libc::ssize_t) -> io::Result<usize> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as usize)
    }
}

/// A descriptor the kernel just returned.
fn owned(raw: libc::c_int) -> io::Result<OwnedFd> {
    let raw = cvt(raw)?;
    // SAFETY: `raw` is a non-negative descriptor a system call made in this
    // function's caller just returned: newly installed, known to nothing else
    // in this process.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// `open(path, flags)`.
pub fn open(path: &CStr, flags: i32) -> io::Result<OwnedFd> {
    // SAFETY: `path` is NUL-terminated and outlives the call.
    owned(unsafe { libc::open(path.as_ptr(), flags) })
}

/// `open(path, flags)`, for a path held as a string.
pub fn open_path(path: &str, flags: i32) -> io::Result<OwnedFd> {
    let c = std::ffi::CString::new(path).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    open(&c, flags)
}

/// `memfd_create(name, flags)`.
pub fn memfd(name: &CStr, flags: libc::c_uint) -> io::Result<OwnedFd> {
    // SAFETY: `name` is NUL-terminated and outlives the call.
    owned(unsafe { libc::memfd_create(name.as_ptr(), flags) })
}

/// `ftruncate(fd, len)`.
pub fn ftruncate(fd: impl AsFd, len: u64) -> io::Result<()> {
    let len = libc::off_t::try_from(len).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: integer arguments on a live descriptor.
    cvt(unsafe { libc::ftruncate(fd.as_fd().as_raw_fd(), len) }).map(|_| ())
}

/// `fcntl(fd, F_ADD_SEALS, seals)`.
pub fn add_seals(fd: impl AsFd, seals: i32) -> io::Result<()> {
    // SAFETY: integer arguments on a live descriptor.
    cvt(unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_ADD_SEALS, seals) }).map(|_| ())
}

/// `fcntl(fd, F_GET_SEALS)`.
pub fn seals(fd: impl AsFd) -> io::Result<i32> {
    // SAFETY: integer arguments on a live descriptor.
    cvt(unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_GET_SEALS) })
}

/// `eventfd(0, flags)`.
pub fn eventfd(flags: i32) -> io::Result<OwnedFd> {
    // SAFETY: integer arguments.
    owned(unsafe { libc::eventfd(0, flags) })
}

/// Add one to an eventfd's counter; a full counter is not an error here.
pub fn eventfd_signal(fd: RawFd) {
    let _ = write_raw(fd, &1u64.to_ne_bytes());
}

/// Read an eventfd's counter back to zero (non-blocking descriptors).
pub fn eventfd_drain(fd: RawFd) {
    let mut b = [0u8; 8];
    let _ = read_raw(fd, &mut b);
}

/// `pipe2(flags)`: (read end, write end).
pub fn pipe2(flags: i32) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut p = [0 as libc::c_int; 2];
    // SAFETY: `p` holds the two ints pipe2 writes.
    cvt(unsafe { libc::pipe2(p.as_mut_ptr(), flags) })?;
    // SAFETY: both descriptors pipe2 just made, known to nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) })
}

/// `read(fd, buf)`.
pub fn read(fd: impl AsFd, buf: &mut [u8]) -> io::Result<usize> {
    read_raw(fd.as_fd().as_raw_fd(), buf)
}

/// `read` on a descriptor number.
pub fn read_raw(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
    cvt_s(unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) })
}

/// `write(fd, buf)`.
pub fn write(fd: impl AsFd, buf: &[u8]) -> io::Result<usize> {
    write_raw(fd.as_fd().as_raw_fd(), buf)
}

/// `write` on a descriptor number.
pub fn write_raw(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: the kernel reads at most `buf.len()` bytes from `buf`.
    cvt_s(unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) })
}

/// `fcntl(fd, F_GETFL)`.
pub fn status_flags(fd: RawFd) -> io::Result<i32> {
    // SAFETY: integer arguments.
    cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })
}

/// Set `O_NONBLOCK` on `fd`.
pub fn set_nonblock(fd: RawFd) -> io::Result<()> {
    let fl = status_flags(fd)?;
    // SAFETY: integer arguments.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) }).map(|_| ())
}

/// Whether `fd` is an open descriptor of this process (`F_GETFD`).
pub fn is_open(fd: RawFd) -> bool {
    // SAFETY: integer arguments; a closed number is EBADF.
    unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 }
}

/// A duplicate of `fd`, close-on-exec, numbered at least `min`.
pub fn dup_at_least(fd: impl AsFd, min: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: integer arguments; the result is a new descriptor.
    owned(unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_DUPFD_CLOEXEC, min) })
}

/// A duplicate of descriptor number `fd`, close-on-exec, for one a library
/// hands out by number only.
pub fn dup_raw(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: integer arguments; the result is a new descriptor.
    owned(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) })
}

/// `lseek(fd, 0, SEEK_END)`: a regular file's or a dma-buf's size.
pub fn size(fd: RawFd) -> io::Result<u64> {
    // SAFETY: integer arguments.
    let r = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as u64)
    }
}

/// `lseek(fd, off, SEEK_DATA)`.
pub fn seek_data(fd: RawFd, off: u64) -> io::Result<u64> {
    let off = libc::off_t::try_from(off).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: integer arguments.
    let r = unsafe { libc::lseek(fd, off, libc::SEEK_DATA) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as u64)
    }
}

/// `fstat(fd)`.
pub fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: an all-zero `stat` is a valid value for fstat to overwrite.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a live, writable stat.
    cvt(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

/// `fstatfs(fd)`'s `f_type`: the magic number of the filesystem the file
/// is on.
pub fn fstatfs_type(fd: RawFd) -> io::Result<i64> {
    // SAFETY: an all-zero `statfs` is a valid value for fstatfs to overwrite.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a live, writable statfs.
    cvt(unsafe { libc::fstatfs(fd, &mut st) })?;
    Ok(st.f_type as i64)
}

/// `epoll_create1(EPOLL_CLOEXEC)`.
pub fn epoll_create() -> io::Result<OwnedFd> {
    // SAFETY: integer arguments.
    owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })
}

/// `epoll_ctl(ep, op, fd, {events, data})`.
pub fn epoll_ctl(ep: RawFd, op: i32, fd: RawFd, events: u32, data: u64) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: data };
    // SAFETY: `ev` is a live epoll_event the kernel only reads.
    cvt(unsafe { libc::epoll_ctl(ep, op, fd, &mut ev) }).map(|_| ())
}

/// `epoll_wait(ep, events, timeout)`: how many of `events` were filled.
pub fn epoll_wait(ep: RawFd, events: &mut [libc::epoll_event], timeout: i32) -> io::Result<usize> {
    let n = i32::try_from(events.len()).unwrap_or(i32::MAX);
    // SAFETY: the kernel writes at most `n` events into `events`.
    cvt(unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), n, timeout) }).map(|n| n as usize)
}

/// `poll(fds, timeout)`: how many have events.
pub fn poll(fds: &mut [libc::pollfd], timeout: i32) -> io::Result<usize> {
    // SAFETY: the kernel reads and writes exactly `fds.len()` pollfds.
    cvt(unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) })
        .map(|n| n as usize)
}

/// Whether `fd` is readable within `timeout_ms` (0: now).
pub fn readable(fd: RawFd, timeout_ms: i32) -> bool {
    let mut p = [libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }];
    matches!(poll(&mut p, timeout_ms), Ok(n) if n > 0) && p[0].revents & libc::POLLIN != 0
}

/// The peer credentials of a Unix socket (`SO_PEERCRED`).
pub fn peer_cred(sock: RawFd) -> io::Result<libc::ucred> {
    // SAFETY: an all-zero ucred is a valid value to overwrite.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are live locals of the sizes given.
    cvt(unsafe {
        libc::getsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    })?;
    Ok(cred)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipe_carries_bytes_and_closes_once() {
        let (r, w) = pipe2(libc::O_CLOEXEC).unwrap();
        assert_eq!(write(&w, b"nvgpu").unwrap(), 5);
        let mut b = [0u8; 8];
        assert_eq!(read(&r, &mut b).unwrap(), 5);
        assert_eq!(&b[..5], b"nvgpu");
        let n = w.as_raw_fd();
        drop(w);
        assert!(!is_open(n) || n == r.as_raw_fd());
    }

    #[test]
    fn an_eventfd_signals_and_drains() {
        let e = eventfd(libc::EFD_CLOEXEC | libc::EFD_NONBLOCK).unwrap();
        assert!(!readable(e.as_raw_fd(), 0));
        eventfd_signal(e.as_raw_fd());
        assert!(readable(e.as_raw_fd(), 0));
        eventfd_drain(e.as_raw_fd());
        assert!(!readable(e.as_raw_fd(), 0));
    }
}
