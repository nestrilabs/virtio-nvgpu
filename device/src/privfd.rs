//! Descriptors the backend holds for itself, as opposed to on a guest's behalf.
//!
//! An IOCTL2 can make the host kernel hand back a descriptor (CREATE_LEASE's
//! lease fd, a syncobj's sync_file, an ATOMIC out-fence), and the backend then
//! adopts whatever number the kernel left at that schema position: it wraps it
//! in an `OwnedFd`, gives it a handle, and closes it when the guest says so.
//! The kernel only ever writes a descriptor it just installed there -- but if
//! a schema is wrong, or a driver copies back a field it was never meant to
//! write, the number could be one this process already has open for its own
//! reasons: the vhost-user socket, the memfd behind guest memory, the event
//! pump's epoll or wake eventfd, a vring's kick eventfd. Adopting one of those
//! would give the guest a handle to it, and its CLOSE would close the socket
//! the whole VM runs on from under the thread using it.
//!
//! The handle table knows its own descriptors. Everything else lives here: a
//! process-wide set of descriptor numbers that belong to the backend itself,
//! which `xfer::Finisher::is_backend_fd` consults alongside the table. It has
//! to be process-wide because most of these descriptors are not the
//! backend's to begin with -- the transport, the pump and the vhost-user
//! library hold them -- and all of them share one descriptor table.
//!
//! Getting it exact means registering while the number is open and
//! unregistering *before* it is closed ([`PrivateFd`] does both): a number
//! still registered after its close could be handed out again by the kernel,
//! and the fresh descriptor would then be refused (a leak and a failed call);
//! a number closed before it is registered cannot be handed to anyone in
//! between, because it is not free. The descriptors a library opens and closes
//! out of sight cannot be tracked one by one, so [`register_process_fds`]
//! takes the whole table once, at a point where nothing but the backend's own
//! plumbing is open.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn set() -> MutexGuard<'static, HashSet<RawFd>> {
    static SET: OnceLock<Mutex<HashSet<RawFd>>> = OnceLock::new();
    SET.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Mark `fd` as the backend's own. Call while it is open.
pub fn register(fd: RawFd) {
    if fd >= 0 {
        set().insert(fd);
    }
}

/// Forget `fd`. Call before it is closed.
pub fn unregister(fd: RawFd) {
    set().remove(&fd);
}

/// Whether `fd` is registered as one of the backend's own descriptors.
pub fn is_private(fd: RawFd) -> bool {
    set().contains(&fd)
}

/// A descriptor the backend owns for itself, registered for as long as it
/// is open.
#[derive(Debug)]
pub struct PrivateFd(OwnedFd);

impl PrivateFd {
    pub fn new(fd: OwnedFd) -> Self {
        register(fd.as_raw_fd());
        Self(fd)
    }

    /// Another descriptor for the same file, registered too.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self::new(self.0.try_clone()?))
    }
}

impl From<OwnedFd> for PrivateFd {
    fn from(fd: OwnedFd) -> Self {
        Self::new(fd)
    }
}

impl AsRawFd for PrivateFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsFd for PrivateFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Drop for PrivateFd {
    /// Unregistered first; the `OwnedFd` field closes after this returns.
    fn drop(&mut self) {
        unregister(self.0.as_raw_fd());
    }
}

/// Register every descriptor open in this process except those `skip`
/// claims (the handle table's). Returns how many were registered.
///
/// For the descriptors the vhost-user library opens and never shows us: the
/// listening and connected sockets, the backend request channel, its worker
/// threads' epoll and exit eventfds -- and stdio and the log, which must not
/// be adoptable either. Call it once the connection is up and before the
/// guest can have made a call, from a thread that is the only one opening
/// anything; every descriptor open then is plumbing.
pub fn register_process_fds(skip: &dyn Fn(RawFd) -> bool) -> usize {
    let fds = open_fds(skip);
    fds.iter().for_each(|&fd| register(fd));
    fds.len()
}

/// Every descriptor open in this process that `skip` does not claim.
///
/// The directory stream reading `/proc/self/fd` holds a descriptor of its own
/// while it is read. Every number is therefore checked again once the stream
/// is closed, and one that no longer names an open file is left out.
pub fn open_fds(skip: &dyn Fn(RawFd) -> bool) -> Vec<RawFd> {
    let Ok(dir) = std::fs::read_dir("/proc/self/fd") else {
        return Vec::new();
    };
    let fds: Vec<RawFd> = dir
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    fds.into_iter()
        .filter(|&fd| {
            // F_GETFD only reads the descriptor's flags; a closed number
            // answers EBADF.
            crate::sys::fd::is_open(fd) && !skip(fd)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A descriptor at a number far above what the other tests in this
    /// process open, so a parallel test cannot recycle it between the drop
    /// and the check below.
    fn high_fd(min: RawFd) -> OwnedFd {
        let ev = crate::hostfd::new_eventfd().unwrap();
        let fd = crate::sys::fd::dup_at_least(&ev, min).unwrap();
        assert!(fd.as_raw_fd() >= min);
        fd
    }

    #[test]
    fn a_private_fd_is_registered_exactly_while_it_is_open() {
        let p = PrivateFd::new(high_fd(3000));
        let n = p.as_raw_fd();
        assert!(is_private(n));
        let q = p.try_clone().unwrap();
        assert!(is_private(q.as_raw_fd()));
        drop(p);
        assert!(!is_private(n), "unregistered before its number is free");
        assert!(!crate::sys::fd::is_open(n), "and closed");
        drop(q);
    }

    /// Only the listing is tested: registering the whole table here would
    /// register every other test's descriptors too, and leave them
    /// registered after those tests close them.
    #[test]
    #[cfg_attr(miri, ignore = "Miri has no /proc/self/fd")]
    fn the_process_listing_has_what_is_open_and_not_what_it_is_told_to_skip() {
        let kept = high_fd(3100);
        let skipped = high_fd(3200);
        let (k, s) = (kept.as_raw_fd(), skipped.as_raw_fd());
        let fds = open_fds(&|fd| fd == s);
        assert!(fds.contains(&k));
        assert!(!fds.contains(&s));
        drop(kept);
        assert!(
            !open_fds(&|_| false).contains(&k),
            "a closed number is not listed"
        );
    }
}
