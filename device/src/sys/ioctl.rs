//! The host kernel's `ioctl`, reachable only with an argument an
//! [`Arena`](super::block::Arena) built.

use std::io;
use std::os::fd::RawFd;

use super::block::{Arg, Kernel};

/// The real kernel.
pub struct Host;

impl Kernel for Host {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut Arg<'_>) -> i32 {
        // SAFETY: an `Arg` is made only by `Arena::call` (or `block::flat`),
        // which checked that it addresses at least `_IOC_SIZE(request)` bytes
        // the arena owns and keeps mapped for the call. Every field of it the
        // backend declared a pointer holds 0 or the address of another block
        // of that arena or of a `HostSpan` it holds -- memory no Rust
        // reference covers while the call runs -- and the arena writes no
        // other address into a block (block.rs). What the host follows
        // beyond the declared fields is what the ABI tables say it does not.
        let r = unsafe { libc::ioctl(fd, request as libc::Ioctl, arg.as_mut_ptr()) };
        if r < 0 {
            -io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
        } else {
            r
        }
    }
}

/// The real kernel, retrying a call a signal interrupted, as libdrm's
/// `drmIoctl` does.
pub struct HostRetry;

impl Kernel for HostRetry {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut Arg<'_>) -> i32 {
        loop {
            let r = Host.ioctl(fd, request, arg);
            if r != -libc::EINTR {
                return r;
            }
        }
    }
}

/// An ioctl with no argument (`_IOC_SIZE` 0): its result, or the error.
/// Refused for a request that says it carries one.
pub fn no_arg(fd: RawFd, request: u64) -> io::Result<i32> {
    if crate::hostfd::ioc_size(request as u32) != 0 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // SAFETY: the request carries no argument, so the kernel copies nothing
    // through the null pointer (and a handler that tried would get EFAULT).
    let r = unsafe { libc::ioctl(fd, request as libc::Ioctl, std::ptr::null_mut::<u8>()) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// `UDMABUF_CREATE` (include/uapi/linux/udmabuf.h): a dma-buf of `size`
/// bytes of `memfd` from offset 0, which the ioctl returns as its result.
pub fn udmabuf_create(
    dev: std::os::fd::BorrowedFd<'_>,
    memfd: std::os::fd::BorrowedFd<'_>,
    size: u64,
    flags: u32,
) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    const UDMABUF_CREATE: u32 = crate::hostfd::ioc(crate::hostfd::IOC_W, b'u', 0x42, 24);
    // struct udmabuf_create { u32 memfd; u32 flags; u64 offset; u64 size; }
    let mut create = [0u8; 24];
    create[0..4].copy_from_slice(&(memfd.as_raw_fd() as u32).to_le_bytes());
    create[4..8].copy_from_slice(&flags.to_le_bytes());
    create[16..24].copy_from_slice(&size.to_le_bytes());
    // SAFETY: `create` is the whole 24-byte struct udmabuf_create, which
    // holds no pointer; the kernel only reads it.
    let r = unsafe {
        libc::ioctl(
            dev.as_raw_fd(),
            UDMABUF_CREATE as libc::Ioctl,
            create.as_mut_ptr(),
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: UDMABUF_CREATE returns the new dma-buf's descriptor, just
    // installed for this process and known to nothing else.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(r) })
}
