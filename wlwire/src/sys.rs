// SPDX-License-Identifier: Apache-2.0
//! The handful of system calls both ends need, over `libc`, so the guest
//! daemon stays a small static binary.
//!
//! The only module of this crate with `unsafe` in it (lib.rs forbids it
//! everywhere else; scripts/check-unsafe.sh holds the tree to that). Each
//! block says what makes it sound: every buffer handed to the kernel is a
//! live slice or local of the length given, and every descriptor wrapped as
//! owned is one a call just returned.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

use crate::wire::MAX_FDS_PER_SENDMSG;

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

#[cfg(not(miri))]
pub fn memfd(name: &CStr, size: u64) -> io::Result<OwnedFd> {
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let fd = cvt(unsafe {
        libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
    })?;
    // SAFETY: a descriptor memfd_create just returned, known to nothing else.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if size > 0 {
        ftruncate(fd.as_raw_fd(), size)?;
    }
    Ok(fd)
}

/// Under Miri, which has no `memfd_create`: an unlinked temporary file,
/// which holds bytes, a size and a file offset as a memfd does. What it
/// cannot do -- seals, punched holes -- the two functions below stand in for.
#[cfg(miri)]
pub fn memfd(_name: &CStr, size: u64) -> io::Result<OwnedFd> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "wlwire-miri-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    f.set_len(size)?;
    Ok(f.into())
}

pub fn ftruncate(fd: RawFd, size: u64) -> io::Result<()> {
    // SAFETY: integer arguments.
    cvt(unsafe { libc::ftruncate(fd, size as libc::off_t) }).map(|_| ())
}

/// Free the pages of `fd` in `[off, off + len)`, keeping its size: they read
/// as zeros after, and hold no memory until written again.
#[cfg(not(miri))]
pub fn punch_hole(fd: RawFd, off: u64, len: u64) -> io::Result<()> {
    // SAFETY: integer arguments.
    cvt(unsafe {
        libc::fallocate(
            fd,
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            off as libc::off_t,
            len as libc::off_t,
        )
    })
    .map(|_| ())
}

/// Under Miri: zeros written over the range, which reads as a hole does.
#[cfg(miri)]
pub fn punch_hole(fd: RawFd, off: u64, len: u64) -> io::Result<()> {
    let end = off.saturating_add(len).min(file_size(fd)?);
    if end > off {
        pwrite_full(fd, &vec![0u8; (end - off) as usize], off)?;
    }
    Ok(())
}

pub fn page_size() -> u64 {
    // SAFETY: an integer argument; no memory is touched.
    match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 => n as u64,
        _ => 4096,
    }
}

/// Seal a finished blob so the receiver can trust its size and contents.
#[cfg(not(miri))]
pub fn seal_readonly(fd: RawFd) -> io::Result<()> {
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
    // SAFETY: integer arguments.
    cvt(unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) }).map(|_| ())
}

/// Under Miri, which has no seals: nothing.
#[cfg(miri)]
pub fn seal_readonly(_fd: RawFd) -> io::Result<()> {
    Ok(())
}

pub fn file_size(fd: RawFd) -> io::Result<u64> {
    Ok(fstat(fd)?.st_size as u64)
}

pub fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: an all-zero stat is a valid value for fstat to overwrite.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a live, writable stat.
    cvt(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

/// `lseek(fd, off, SEEK_DATA)`: where data starts at or after `off`.
pub fn seek_data(fd: RawFd, off: u64) -> io::Result<u64> {
    // SAFETY: integer arguments.
    let r = unsafe { libc::lseek(fd, off as libc::off_t, libc::SEEK_DATA) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as u64)
    }
}

/// `fcntl(fd, F_GET_SEALS)`.
pub fn seals(fd: RawFd) -> io::Result<i32> {
    // SAFETY: integer arguments.
    cvt(unsafe { libc::fcntl(fd, libc::F_GET_SEALS) })
}

/// Read up to `buf.len()` bytes at `off`, stopping early only at end of file.
pub fn pread_full(fd: RawFd, buf: &mut [u8], off: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        // SAFETY: the kernel writes at most `buf.len() - done` bytes into
        // `buf[done..]`.
        let r = unsafe {
            libc::pread(
                fd,
                buf[done..].as_mut_ptr().cast(),
                buf.len() - done,
                (off + done as u64) as libc::off_t,
            )
        };
        match cvt_s(r) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

pub fn pwrite_full(fd: RawFd, buf: &[u8], off: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        // SAFETY: the kernel reads at most `buf.len() - done` bytes of
        // `buf[done..]`.
        let r = unsafe {
            libc::pwrite(
                fd,
                buf[done..].as_ptr().cast(),
                buf.len() - done,
                (off + done as u64) as libc::off_t,
            )
        };
        match cvt_s(r) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
    cvt_s(unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) })
}

pub fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: the kernel reads at most `buf.len()` bytes of `buf`.
    cvt_s(unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) })
}

/// (read end, write end), both close-on-exec; the read end non-blocking.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut p = [0; 2];
    // SAFETY: `p` holds the two ints pipe2 writes.
    cvt(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) })?;
    // SAFETY: both descriptors pipe2 just made, known to nothing else.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) };
    set_nonblock(r.as_raw_fd())?;
    Ok((r, w))
}

pub fn set_nonblock(fd: RawFd) -> io::Result<()> {
    // SAFETY: integer arguments.
    let fl = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    // SAFETY: integer arguments.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) }).map(|_| ())
}

pub fn eventfd() -> io::Result<OwnedFd> {
    // SAFETY: integer arguments.
    let fd = cvt(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
    // SAFETY: a descriptor eventfd just returned, known to nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn eventfd_signal(fd: RawFd) {
    let _ = write(fd, &1u64.to_ne_bytes());
}

/// Reset an eventfd to "not readable".
pub fn eventfd_clear(fd: RawFd) {
    let mut v = [0u8; 8];
    let _ = read(fd, &mut v);
}

pub fn dup(fd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    fd.try_clone_to_owned()
}

/// `sendmsg` with at most [`MAX_FDS_PER_SENDMSG`] descriptors, non-blocking,
/// no SIGPIPE. Returns the bytes written; the descriptors went with the first
/// byte if any were written.
pub fn send_with_fds(sock: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    assert!(fds.len() <= MAX_FDS_PER_SENDMSG);
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut _,
        iov_len: data.len(),
    };
    let mut cbuf = [0u64; 32]; // room for CMSG_SPACE(28 * 4)
    // SAFETY: an all-zero msghdr is a valid value; the fields used are set.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        // SAFETY: arithmetic on an integer argument.
        let len = unsafe { libc::CMSG_SPACE((fds.len() * 4) as u32) } as usize;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = len as _;
        // SAFETY: `cbuf` (256 bytes, 8-aligned) holds CMSG_SPACE of at most
        // MAX_FDS_PER_SENDMSG descriptors (asserted above), so the first
        // header and its data, of `fds.len()` ints, lie inside it.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(c).cast::<RawFd>(),
                fds.len(),
            );
        }
    }
    // SAFETY: `msg` names `iov` (the live `data`) and `cbuf`, both of the
    // lengths given; the kernel only reads them.
    cvt_s(unsafe { libc::sendmsg(sock, &msg, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) })
}

/// `recvmsg` into `buf`, appending any descriptors received to `fds`.
/// Returns 0 at end of stream.
pub fn recv_with_fds(sock: RawFd, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // Room for more than libwayland ever sends at once, so nothing is
    // truncated by us.
    let mut cbuf = [0u64; 64];
    // SAFETY: an all-zero msghdr is a valid value; the fields used are set.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    // SAFETY: the kernel writes at most `buf.len()` bytes through `iov` and
    // `msg_controllen` bytes into `cbuf`, both live for the call.
    let n = cvt_s(unsafe {
        libc::recvmsg(sock, &mut msg, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC)
    })?;
    // SAFETY: the kernel filled `cbuf` with well-formed control messages of
    // `msg_controllen` bytes, which CMSG_FIRSTHDR/NXTHDR walk without leaving
    // it; each SCM_RIGHTS payload is `count` descriptors the kernel just
    // installed in this process for this call (close-on-exec), so each is
    // wrapped as owned exactly once.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("descriptors truncated (MSG_CTRUNC)"));
    }
    Ok(n)
}

/// An empty sealed memfd, handed to a peer in place of a descriptor that could
/// not be carried, so the message still consumes one.
pub fn placeholder_fd() -> io::Result<OwnedFd> {
    let fd = memfd(c"nvgpu-wl-invalid", 0)?;
    seal_readonly(fd.as_raw_fd())?;
    Ok(fd)
}

pub fn is_fifo(fd: RawFd) -> bool {
    fstat(fd)
        .map(|st| st.st_mode & libc::S_IFMT == libc::S_IFIFO)
        .unwrap_or(false)
}

pub fn is_regular(fd: RawFd) -> bool {
    fstat(fd)
        .map(|st| st.st_mode & libc::S_IFMT == libc::S_IFREG)
        .unwrap_or(false)
}

/// `statfs.f_type` of tmpfs (and of every memfd), and of hugetlbfs (a
/// hugetlb memfd).
const TMPFS_MAGIC: i64 = 0x0102_1994;
const HUGETLBFS_MAGIC: i64 = 0x9584_58f6;

/// A regular file whose pages are memory: a memfd, or a file on tmpfs or
/// hugetlbfs. A `pread` of it never waits on anyone. A file on FUSE, NFS or a
/// device can make it wait for as long as its server likes, and the proxy
/// reads shm pools and blobs on the thread that serves every other message
/// of the connection -- in the backend, under the lock WL_SEND and WL_RECV
/// take.
#[cfg(not(miri))]
pub fn is_shmem(fd: RawFd) -> bool {
    // SAFETY: an all-zero statfs is a valid value for fstatfs to overwrite.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a live, writable statfs.
    if cvt(unsafe { libc::fstatfs(fd, &mut st) }).is_err() {
        return false;
    }
    let magic = st.f_type as i64;
    is_regular(fd) && (magic == TMPFS_MAGIC || magic == HUGETLBFS_MAGIC)
}

/// Under Miri, whose "memfd" is a temporary file (above), and which has no
/// `fstatfs`: any regular file.
#[cfg(miri)]
pub fn is_shmem(fd: RawFd) -> bool {
    let _ = (TMPFS_MAGIC, HUGETLBFS_MAGIC);
    is_regular(fd)
}
