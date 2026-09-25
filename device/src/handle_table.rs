//! Backend handles: the only names a guest has for host descriptors.
//!
//! --- Handle lifecycle and ungraceful teardown ---
//!
//! Normal path: guest calls close() → nv_release() → NV_MSG_CLOSE →
//!   `HandleTable::remove()` hands back the `OwnedFd`, which closes the host
//!   fd when dropped. The host NVIDIA driver sees the close() and frees its
//!   internal state.
//!
//! Ungraceful path (VM crash, guest reboot, driver reload): the session is
//!   reset -- by `reset_device`, or by a HELLO carrying `HELLO_F_FRESH` -- and
//!   `drain_all()` drops every `OwnedFd` in one sweep. The host driver's
//!   fd-release path runs for each one, freeing every RM object, dropping DRM
//!   master, ending leases. This is analogous to nvproxy's Release() in
//!   pkg/sentry/devices/nvproxy/nvproxy.go.
//!
//! --- Why u32, and why cyclic ---
//!
//! A handle is a `u32` on the wire, in the guest, and in the event pump's
//! maps. It used to be a `u64` counter truncated to `u32` wherever it left
//! this table, which is fine at a few opens per second and not at the rate
//! protocol v2 creates them: a sync_file per commit, an eventfd per syncobj
//! wait, a dmabuf per Wayland buffer. At a thousand a second the counter
//! passes 2^32 in about fifty days, and from then on a new handle's low bits
//! alias a long-lived one -- a card that is DRM master, the control device, a
//! watched fd.
//!
//! So the table allocates in `u32` directly, cyclically, skipping 0 (the
//! "none" value everywhere on the wire), `!0` (what an unset field reads as),
//! and every live value. A stale handle can then only name a new object after
//! the whole space has gone round once, and it can never name a live one. The
//! counter deliberately survives `drain_all`: a guest that restarts has no
//! business holding an old handle, but a late executor completion from before
//! the reset might, and it must not find a new object under it.
//!
//! The table is bounded ([`MAX_HANDLES`]) and says -EMFILE when full, the way
//! the host would say it to a process that opened too much.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use crate::error::{DeviceError, Result};
use crate::hostfd::HandleKind;

/// Most handles one session may hold at once.
///
/// The host's own per-process limit is what really bounds this backend, and
/// the backend serves every guest process from one host process. 65536 is far
/// beyond what one guest opens in practice (a busy compositor with a few
/// dozen clients holds a few hundred), and small enough that a guest looping
/// on HOST_OP NEW_EVENTFD runs out here, with an error it can report, rather
/// than in the host's fd table, where it takes every other guest down too.
pub const MAX_HANDLES: usize = 65536;

/// Why a handle could not be issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableFull;

impl TableFull {
    /// The errno a guest sees.
    pub fn errno(self) -> i32 {
        libc::EMFILE
    }
}

struct Entry {
    fd: OwnedFd,
    kind: HandleKind,
    /// The host file was closed under the guest (`bury`).
    buried: bool,
}

pub struct HandleTable {
    /// Where the search for the next free value starts.
    next: u32,
    table: HashMap<u32, Entry>,
    limit: usize,
}

impl HandleTable {
    pub fn new() -> Self {
        Self::with_limit(MAX_HANDLES)
    }

    /// A table that holds at most `limit` handles. For tests that want to
    /// reach the limit without opening 65536 descriptors.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            next: 1,
            table: HashMap::new(),
            limit,
        }
    }

    /// Take ownership of `fd` and issue a handle for it.
    ///
    /// When the table is full the descriptor is dropped, i.e. closed: there is
    /// nobody left to hand it to, and a descriptor that nobody can name is a
    /// leak.
    pub fn insert(&mut self, fd: OwnedFd, kind: HandleKind) -> std::result::Result<u32, TableFull> {
        if self.table.len() >= self.limit {
            return Err(TableFull);
        }
        // Terminates: fewer than 2^32 - 2 values are live, so some candidate
        // is free, and in practice the first one almost always is.
        loop {
            let h = self.next;
            self.next = self.next.wrapping_add(1);
            if h == 0 || h == u32::MAX || self.table.contains_key(&h) {
                continue;
            }
            self.table.insert(
                h,
                Entry {
                    fd,
                    kind,
                    buried: false,
                },
            );
            return Ok(h);
        }
    }

    /// The raw fd behind `handle`, borrowed for the length of a call made
    /// while the table cannot change underneath it.
    pub fn get_raw(&self, handle: u32) -> Result<RawFd> {
        self.table
            .get(&handle)
            .map(|e| e.fd.as_raw_fd())
            .ok_or(DeviceError::BadHandle(handle as u64))
    }

    /// The descriptor and its kind.
    pub fn get(&self, handle: u32) -> Option<(BorrowedFd<'_>, HandleKind)> {
        self.table.get(&handle).map(|e| (e.fd.as_fd(), e.kind))
    }

    /// What `handle` is, if it exists.
    pub fn kind(&self, handle: u32) -> Option<HandleKind> {
        self.table.get(&handle).map(|e| e.kind)
    }

    /// A duplicate of the descriptor, `O_CLOEXEC`, so a call can keep using
    /// it after the table lock is dropped even if the guest closes the handle
    /// meanwhile.
    pub fn dup(&self, handle: u32) -> Option<(OwnedFd, HandleKind)> {
        let e = self.table.get(&handle)?;
        let fd = e.fd.try_clone().ok()?;
        Some((fd, e.kind))
    }

    /// Whether `fd` is one of ours. A descriptor number the host hands back
    /// is only adopted when it is not: adopting one we already hold would put
    /// it under two handles, and the second close would close someone else's.
    pub fn owns_fd(&self, fd: RawFd) -> bool {
        self.table.values().any(|e| e.fd.as_raw_fd() == fd)
    }

    /// Close the host file behind `handle` without ending the handle: the
    /// descriptor is swapped for `stub` (an eventfd nobody signals) and the
    /// kind becomes `Other`, so nothing can use it again, and the guest,
    /// which still holds the number, closes it as it would any other. The
    /// old descriptor is returned (it closes when dropped). For a lease file
    /// whose lease the host ended (kms.rs, "lease ends").
    pub fn bury(&mut self, handle: u32, stub: OwnedFd) -> Result<OwnedFd> {
        let e = self
            .table
            .get_mut(&handle)
            .ok_or(DeviceError::BadHandle(handle as u64))?;
        e.kind = HandleKind::Other;
        e.buried = true;
        Ok(std::mem::replace(&mut e.fd, stub))
    }

    /// Whether `handle`'s host file was closed under it (`bury`): a call on
    /// it is answered ENODEV, as a file whose device went away is.
    pub fn is_buried(&self, handle: u32) -> bool {
        self.table.get(&handle).is_some_and(|e| e.buried)
    }

    /// Remove `handle`, returning its descriptor (which closes when dropped).
    pub fn remove(&mut self, handle: u32) -> Result<(OwnedFd, HandleKind)> {
        self.table
            .remove(&handle)
            .map(|e| (e.fd, e.kind))
            .ok_or(DeviceError::BadHandle(handle as u64))
    }

    /// Every live handle.
    pub fn handles(&self) -> Vec<u32> {
        self.table.keys().copied().collect()
    }

    /// Close every open fd and empty the table (session reset / teardown).
    ///
    /// Dropping each `OwnedFd` calls close(2) on the host fd. The host NVIDIA
    /// driver's release() path then frees all RM objects for that fd, DRM
    /// drops master and ends any lease the file held.
    pub fn drain_all(&mut self) {
        let count = self.table.len();
        if count > 0 {
            log::info!("HandleTable::drain_all: closing {count} host fds");
        }
        for (handle, e) in self.table.drain() {
            log::debug!(
                "  closing handle={handle} ({:?}) host_fd={}",
                e.kind,
                e.fd.as_raw_fd()
            );
        }
    }

    /// Number of open handles.
    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buried_handle_keeps_its_number_but_not_its_file_until_it_is_removed() {
        let mut t = HandleTable::new();
        let fd = crate::hostfd::new_eventfd().unwrap();
        let raw = fd.as_raw_fd();
        let h = t.insert(fd, HandleKind::DrmLease(0)).unwrap();
        assert!(!t.is_buried(h));
        let stub = crate::hostfd::new_eventfd().unwrap();
        let stub_raw = stub.as_raw_fd();
        let old = t.bury(h, stub).unwrap();
        assert_eq!(
            old.as_raw_fd(),
            raw,
            "the host file comes back, to be closed"
        );
        drop(old);
        assert!(t.is_buried(h));
        assert_eq!(t.kind(h), Some(HandleKind::Other), "usable as nothing");
        assert_eq!(t.get_raw(h).unwrap(), stub_raw);
        assert!(
            t.bury(h + 1, crate::hostfd::new_eventfd().unwrap())
                .is_err()
        );
        t.remove(h).unwrap();
        assert!(!t.is_buried(h));
    }
    use protocol::messages::DeviceKind;
    use std::os::fd::FromRawFd;

    fn make_fd() -> OwnedFd {
        // SAFETY: a NUL-terminated path; the fd is owned by the returned value.
        let raw = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        assert!(raw >= 0);
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    const CTL: HandleKind = HandleKind::Dev(DeviceKind::Ctl);

    #[test]
    fn a_handle_names_its_descriptor_and_kind_until_it_is_removed() {
        let mut t = HandleTable::new();
        let h = t.insert(make_fd(), CTL).unwrap();
        assert!(h > 0);
        assert!(t.get_raw(h).is_ok());
        assert_eq!(t.kind(h), Some(CTL));
        assert!(t.remove(h).is_ok());
        assert!(matches!(t.get_raw(h), Err(DeviceError::BadHandle(_))));
        assert_eq!(t.kind(h), None);
    }

    #[test]
    fn handles_are_unique() {
        let mut t = HandleTable::new();
        let h1 = t.insert(make_fd(), CTL).unwrap();
        let h2 = t.insert(make_fd(), CTL).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn drain_all_empties_table() {
        let mut t = HandleTable::new();
        for _ in 0..3 {
            t.insert(make_fd(), CTL).unwrap();
        }
        assert_eq!(t.len(), 3);
        t.drain_all();
        assert_eq!(t.len(), 0);
    }

    /// A late completion from before a reset may still quote an old handle;
    /// it must find nothing rather than whatever the new session opened.
    #[test]
    fn the_counter_keeps_running_across_a_drain_so_old_handles_stay_dead() {
        let mut t = HandleTable::new();
        let old = t.insert(make_fd(), CTL).unwrap();
        t.drain_all();
        let new = t.insert(make_fd(), CTL).unwrap();
        assert_ne!(new, old);
        assert!(matches!(t.get_raw(old), Err(DeviceError::BadHandle(_))));
    }

    #[test]
    fn allocation_wraps_and_skips_zero_all_ones_and_live_handles() {
        let mut t = HandleTable::new();
        let live = t.insert(make_fd(), CTL).unwrap();
        assert_eq!(live, 1);
        // Force the cursor to the top of the space.
        t.next = u32::MAX - 1;
        assert_eq!(t.insert(make_fd(), CTL).unwrap(), u32::MAX - 1);
        // u32::MAX and 0 are never issued, and 1 is live: the next one is 2.
        assert_eq!(t.insert(make_fd(), CTL).unwrap(), 2);
        assert!(t.kind(0).is_none() && t.kind(u32::MAX).is_none());
    }

    #[test]
    fn a_full_table_refuses_with_emfile_and_closes_the_descriptor() {
        let mut t = HandleTable::with_limit(2);
        t.insert(make_fd(), CTL).unwrap();
        t.insert(make_fd(), CTL).unwrap();
        let fd = make_fd();
        let raw = fd.as_raw_fd();
        let err = t.insert(fd, CTL).unwrap_err();
        assert_eq!(err.errno(), libc::EMFILE);
        // The refused descriptor was closed, not leaked.
        assert_eq!(unsafe { libc::fcntl(raw, libc::F_GETFD) }, -1);
        // Space comes back when a handle goes.
        let h = t.handles()[0];
        t.remove(h).unwrap();
        assert!(t.insert(make_fd(), CTL).is_ok());
    }

    #[test]
    fn a_duplicate_outlives_the_handle_it_came_from() {
        let mut t = HandleTable::new();
        let h = t.insert(make_fd(), CTL).unwrap();
        let (dup, kind) = t.dup(h).unwrap();
        assert_eq!(kind, CTL);
        t.remove(h).unwrap();
        assert!(unsafe { libc::fcntl(dup.as_raw_fd(), libc::F_GETFD) } >= 0);
    }

    #[test]
    fn the_table_knows_which_raw_descriptors_it_owns() {
        let mut t = HandleTable::new();
        let fd = make_fd();
        let raw = fd.as_raw_fd();
        t.insert(fd, CTL).unwrap();
        assert!(t.owns_fd(raw));
        assert!(!t.owns_fd(-1));
    }
}
