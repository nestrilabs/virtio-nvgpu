//! Netlink: the kernel's uevent broadcast (kms.rs's hotplug listener).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

fn nl_addr(pid: u32, groups: u32) -> libc::sockaddr_nl {
    // SAFETY: an all-zero sockaddr_nl is a valid value (its padding is
    // zeroed too); the fields that matter are set below.
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    sa.nl_pid = pid;
    sa.nl_groups = groups;
    sa
}

const NL_LEN: libc::socklen_t = size_of::<libc::sockaddr_nl>() as libc::socklen_t;

/// A non-blocking, close-on-exec NETLINK_KOBJECT_UEVENT socket bound to
/// `groups`, with a receive buffer of `rcvbuf` bytes asked for (best effort).
pub fn uevent_socket(groups: u32, rcvbuf: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: integer arguments.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor socket() just returned, known to nothing else.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: setsockopt reads a live local int of the size given.
    unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&rcvbuf as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    let sa = nl_addr(0, groups);
    // SAFETY: bind reads a live sockaddr_nl of the size given.
    let r = unsafe {
        libc::bind(
            sock.as_raw_fd(),
            (&sa as *const libc::sockaddr_nl).cast(),
            NL_LEN,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

/// One datagram into `buf`: its length and the sender's netlink port (0 is
/// the kernel).
pub fn recv_from(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, u32)> {
    let mut sa = nl_addr(0, 0);
    let mut len = NL_LEN;
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf` and at
    // most `len` bytes into `sa`, both live and owned here.
    let n = unsafe {
        libc::recvfrom(
            fd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            (&mut sa as *mut libc::sockaddr_nl).cast(),
            &mut len,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((n as usize, sa.nl_pid))
}

/// The port the kernel gave `fd`.
pub fn local_port(fd: RawFd) -> io::Result<u32> {
    let mut sa = nl_addr(0, 0);
    let mut len = NL_LEN;
    // SAFETY: the kernel writes at most `len` bytes into `sa`, a live local.
    let r = unsafe { libc::getsockname(fd, (&mut sa as *mut libc::sockaddr_nl).cast(), &mut len) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sa.nl_pid)
}

/// Send `msg` from `fd` to netlink port `port`.
pub fn send_to(fd: RawFd, port: u32, msg: &[u8]) -> io::Result<usize> {
    let to = nl_addr(port, 0);
    // SAFETY: the kernel reads `msg.len()` bytes of `msg` and one
    // sockaddr_nl, both live.
    let n = unsafe {
        libc::sendto(
            fd,
            msg.as_ptr().cast(),
            msg.len(),
            0,
            (&to as *const libc::sockaddr_nl).cast(),
            NL_LEN,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}
