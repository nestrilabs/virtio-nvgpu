// SPDX-License-Identifier: Apache-2.0
//! Every `unsafe` of the guest daemon (lib.rs and main.rs forbid it
//! elsewhere; scripts/check-unsafe.sh holds the tree to that): the ioctls
//! of `/dev/nvgpu-wl` and of a DRM node, epoll, and a descriptor's peer.
//! Each block says what makes it sound.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::uapi;

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// A `/dev/nvgpu-wl` argument the kernel only copies as data: no pointer in
/// it.
///
/// # Safety
/// Implemented only here, for `repr(C)` structs of integers that the
/// guest module reads and writes whole and never dereferences.
pub unsafe trait Plain {}

// SAFETY: integers only (uapi.rs, `struct nvgpu_wl_hello`).
unsafe impl Plain for uapi::Hello {}
// SAFETY: integers only (`struct nvgpu_wl_connect`).
unsafe impl Plain for uapi::Connect {}
// SAFETY: integers only (`struct nvgpu_wl_connect_for`).
unsafe impl Plain for uapi::ConnectFor {}

fn ioc_size(req: libc::c_ulong) -> usize {
    ((req >> 16) & 0x3fff) as usize
}

/// `ioctl(f, req, arg)` for an argument of plain data, of exactly the size
/// `req` says.
pub fn ioctl_plain<T: Plain>(f: &File, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    if ioc_size(req) != size_of::<T>() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // SAFETY: `arg` is a live `T` of the size the request copies each way,
    // holding no pointer (the Plain contract).
    cvt(unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg as *mut T) }).map(|_| ())
}

/// SEND (`frame` is what the kernel reads) or RECV (`frame` is where it
/// writes, at most `frame.len()` bytes): `x.frame` and `x.len` are set here
/// from `frame`, and nowhere else.
pub fn xfer(f: &File, req: libc::c_ulong, frame: &mut [u8], x: &mut uapi::Xfer) -> io::Result<()> {
    if req != uapi::IOC_SEND && req != uapi::IOC_RECV {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    x.frame = frame.as_mut_ptr() as u64;
    x.len = u32::try_from(frame.len()).map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?;
    // SAFETY: `x` is a live Xfer of the size the request copies; the one
    // pointer in it is `frame`'s, exclusively borrowed for the call, and
    // `x.len` its length, which is all the module reads or writes through it
    // (driver/nvgpu_wl.c).
    cvt(unsafe { libc::ioctl(f.as_raw_fd(), req as _, x as *mut uapi::Xfer) }).map(|_| ())
}

/// A descriptor number RECV's descriptor table carries: one the guest
/// module installed in this process for this call (close-on-exec), which
/// nothing else knows of. Called once per entry.
pub fn received_fd(fd: RawFd) -> Option<OwnedFd> {
    // SAFETY: the module's RECV installs each descriptor it reports, for
    // this caller, and reports each once; -1 is "none" and is not wrapped.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The driver name `DRM_IOCTL_VERSION` reports for `fd`.
pub fn drm_driver_name(fd: RawFd) -> Option<Vec<u8>> {
    #[repr(C)]
    struct DrmVersion {
        major: i32,
        minor: i32,
        patch: i32,
        name_len: usize,
        name: *mut u8,
        date_len: usize,
        date: *mut u8,
        desc_len: usize,
        desc: *mut u8,
    }
    let mut name = [0u8; 32];
    let mut v = DrmVersion {
        major: 0,
        minor: 0,
        patch: 0,
        name_len: name.len(),
        name: name.as_mut_ptr(),
        date_len: 0,
        date: std::ptr::null_mut(),
        desc_len: 0,
        desc: std::ptr::null_mut(),
    };
    // DRM_IOCTL_VERSION = _IOWR('d', 0x00, struct drm_version)
    let req = (3u64 << 30) | ((size_of::<DrmVersion>() as u64) << 16) | ((b'd' as u64) << 8);
    // SAFETY: `v` is a live drm_version; its one non-null pointer is `name`,
    // live for the call, with `name_len` its length (the kernel copies at
    // most that); the other two lengths are 0.
    let r = unsafe { libc::ioctl(fd, req as _, &mut v) };
    let len = v.name_len.min(name.len());
    (r == 0).then(|| name[..len].to_vec())
}

/// `DRM_IOCTL_DROP_MASTER` (`_IO('d', 0x1f)`): no argument.
pub fn drop_master(fd: RawFd) {
    // SAFETY: the request takes no argument; nothing is copied.
    unsafe { libc::ioctl(fd, ((b'd' as u64) << 8 | 0x1f) as _) };
}

/// `epoll_create1(EPOLL_CLOEXEC)`.
pub fn epoll_create() -> io::Result<OwnedFd> {
    // SAFETY: integer arguments.
    let ep = cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
    // SAFETY: a descriptor epoll_create1 just returned, known to nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(ep) })
}

/// `epoll_ctl(ep, op, fd, {events, token})`.
pub fn epoll_ctl(ep: RawFd, op: i32, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: token };
    // SAFETY: `ev` is a live epoll_event the kernel only reads.
    cvt(unsafe { libc::epoll_ctl(ep, op, fd, &mut ev) }).map(|_| ())
}

/// `epoll_wait(ep, evs, timeout)`: how many of `evs` were filled.
pub fn epoll_wait(ep: RawFd, evs: &mut [libc::epoll_event], timeout: i32) -> io::Result<usize> {
    let n = i32::try_from(evs.len()).unwrap_or(i32::MAX);
    // SAFETY: the kernel writes at most `n` events into `evs`.
    cvt(unsafe { libc::epoll_wait(ep, evs.as_mut_ptr(), n, timeout) }).map(|n| n as usize)
}

/// The peer credentials of a Unix socket (SO_PEERCRED).
pub fn peer_cred(sock: RawFd) -> Option<libc::ucred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`, a live local.
    let r = unsafe {
        libc::getsockopt(
            sock,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(cred)
}

/// Run `handler` on SIGINT and SIGTERM; it must be async-signal-safe (an
/// atomic store).
pub fn on_terminate(handler: extern "C" fn(libc::c_int)) {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: a handler of the plain signature, whose body the caller
        // keeps async-signal-safe.
        unsafe { libc::signal(sig, handler as *const () as libc::sighandler_t) };
    }
}
