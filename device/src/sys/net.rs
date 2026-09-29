// SPDX-License-Identifier: Apache-2.0
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

// ─────────── the capture-injection socket (inject/server.rs) ───────────

fn unix_addr(path: &std::path::Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: an all-zero sockaddr_un is a valid value; the family and path
    // are set below.
    let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let p = path.as_os_str().as_bytes();
    // One byte is kept for the NUL the zeroing left.
    if p.is_empty() || p.len() >= sa.sun_path.len() || p.contains(&0) {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    for (d, s) in sa.sun_path.iter_mut().zip(p) {
        *d = *s as libc::c_char;
    }
    let len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + p.len() + 1) as libc::socklen_t;
    Ok((sa, len))
}

/// A close-on-exec `SOCK_SEQPACKET` Unix socket bound at `path` and
/// listening with a backlog of `backlog`. The caller chooses where (a
/// private directory, then a rename: sockpath.rs).
pub fn seqpacket_listen(path: &std::path::Path, backlog: i32) -> io::Result<OwnedFd> {
    let (sa, len) = unix_addr(path)?;
    // SAFETY: integer arguments.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor socket() just returned, known to nothing else.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: bind reads a live sockaddr_un of the length given.
    if unsafe {
        libc::bind(
            sock.as_raw_fd(),
            (&sa as *const libc::sockaddr_un).cast(),
            len,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: integer arguments.
    if unsafe { libc::listen(sock.as_raw_fd(), backlog) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

/// A close-on-exec `SOCK_SEQPACKET` connection to `path` (the tests, and a
/// helper written against this crate).
pub fn seqpacket_connect(path: &std::path::Path) -> io::Result<OwnedFd> {
    let (sa, len) = unix_addr(path)?;
    // SAFETY: integer arguments.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor socket() just returned, known to nothing else.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: connect reads a live sockaddr_un of the length given.
    if unsafe {
        libc::connect(
            sock.as_raw_fd(),
            (&sa as *const libc::sockaddr_un).cast(),
            len,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

/// `accept4(listener, SOCK_CLOEXEC)`: the next connection, blocking.
pub fn accept(listener: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: a null address and length ask for no peer address.
    let fd = unsafe {
        libc::accept4(
            listener,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor accept4() just returned, known to nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `shutdown(fd, SHUT_RDWR)`: wakes a thread blocked in `accept` or `recv`
/// on it.
pub fn shutdown(fd: RawFd) -> io::Result<()> {
    // SAFETY: integer arguments.
    if unsafe { libc::shutdown(fd, libc::SHUT_RDWR) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Descriptors one packet may carry here; a packet with more ends its
/// connection (the control buffer would truncate, MSG_CTRUNC).
pub const PACKET_MAX_FDS: usize = 8;

/// One received packet.
#[derive(Debug)]
pub struct Packet {
    /// Bytes of the packet that fit in the buffer.
    pub len: usize,
    /// The packet was longer than the buffer (MSG_TRUNC).
    pub truncated: bool,
    /// More descriptors came than [`PACKET_MAX_FDS`] (MSG_CTRUNC); those
    /// that did fit are in `fds`, the rest the kernel has closed.
    pub fds_truncated: bool,
    pub fds: Vec<OwnedFd>,
}

/// `recvmsg` of one packet into `buf`, blocking, with its descriptors
/// (close-on-exec). `len` 0 and no descriptors is the peer's hangup.
pub fn recv_packet(sock: RawFd, buf: &mut [u8]) -> io::Result<Packet> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // CMSG_SPACE(8 * 4) is 48 on x86-64; 16 words leave room.
    let mut cbuf = [0u64; 16];
    // SAFETY: an all-zero msghdr is a valid value; the fields used are set.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    // SAFETY: arithmetic on an integer argument.
    msg.msg_controllen =
        unsafe { libc::CMSG_SPACE((PACKET_MAX_FDS * size_of::<RawFd>()) as u32) } as _;
    debug_assert!(msg.msg_controllen as usize <= size_of_val(&cbuf));
    // SAFETY: the kernel writes at most `buf.len()` bytes through `iov` and
    // `msg_controllen` bytes into `cbuf`, both live for the call.
    let n = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: the kernel filled `cbuf` with well-formed control messages of
    // `msg_controllen` bytes, which CMSG_FIRSTHDR/NXTHDR walk without leaving
    // it; each SCM_RIGHTS payload is `count` descriptors the kernel just
    // installed in this process for this call, so each is wrapped as owned
    // exactly once.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                let count =
                    ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / size_of::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok(Packet {
        len: n as usize,
        truncated: msg.msg_flags & libc::MSG_TRUNC != 0,
        fds_truncated: msg.msg_flags & libc::MSG_CTRUNC != 0,
        fds,
    })
}

/// `sendmsg` of one packet with `fds` (at most [`PACKET_MAX_FDS`]),
/// blocking, no SIGPIPE.
pub fn send_packet(sock: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    if fds.len() > PACKET_MAX_FDS {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut _,
        iov_len: data.len(),
    };
    let mut cbuf = [0u64; 16];
    // SAFETY: an all-zero msghdr is a valid value; the fields used are set.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        // SAFETY: arithmetic on an integer argument.
        let len = unsafe { libc::CMSG_SPACE(size_of_val(fds) as u32) } as usize;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = len as _;
        // SAFETY: `cbuf` (128 bytes, 8-aligned) holds CMSG_SPACE of at most
        // PACKET_MAX_FDS descriptors (checked above), so the first header and
        // its data lie inside it.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(size_of_val(fds) as u32) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(c).cast::<RawFd>(),
                fds.len(),
            );
        }
    }
    // SAFETY: `msg` names `iov` (the live `data`) and `cbuf`, both of the
    // lengths given; the kernel only reads them.
    let n = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}
