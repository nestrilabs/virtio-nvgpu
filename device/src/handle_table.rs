// SPDX-License-Identifier: Apache-2.0
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
//! The table is bounded ([`MAX_HANDLES`], or less when the backend's
//! RLIMIT_NOFILE is lower: [`limit_for_nofile`]) and says -EMFILE when full,
//! the way the host would say it to a process that opened too much.
//!
//! --- Per guest process ---
//!
//! The table is one pool for every process of the guest, and natively there
//! is no such pool: each process has its descriptors. So each handle is
//! charged to the guest process that caused it ([`Owner`]: the opener, or
//! the owner of the file a call that made it ran on), and one process holds
//! at most a quarter of the table, with the last sixteenth kept for
//! processes that hold little (quota.rs). A process past its share gets
//! EMFILE; the others do not.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

use crate::error::{DeviceError, Result};
use crate::hostfd::HandleKind;
use crate::quota::{Charge, Over, Owner, Pool, Share};

/// Most handles one session may hold at once.
///
/// The host's own per-process limit is what really bounds this backend, and
/// the backend serves every guest process from one host process. 65536 is far
/// beyond what one guest opens in practice (a busy compositor with a few
/// dozen clients holds a few hundred), and small enough that a guest looping
/// on HOST_OP NEW_EVENTFD runs out here, with an error it can report, rather
/// than in the host's fd table, where it takes every other guest down too.
pub const MAX_HANDLES: usize = 65536;

/// Host descriptors the backend keeps for itself beyond the table: the
/// vhost-user socket, guest memory, the vrings' eventfds, the window, the
/// pump's epoll, compositor connections and their reader threads' pipes,
/// log files, and the short-lived descriptors of the calls it makes.
pub const NOFILE_RESERVE: u64 = 1024;

/// The most handles a backend whose RLIMIT_NOFILE is `nofile` can back.
///
/// Each handle is a host descriptor, and each device, render and Wayland
/// handle has a second one, the event pump's duplicate; so half of what is
/// left after the backend's own reserve, and never more than
/// [`MAX_HANDLES`]. A limit so low that nothing is left still gets 64: the
/// table then fails early with EMFILE, as it would have in the host's own
/// table, just with a reason logged.
pub fn limit_for_nofile(nofile: u64) -> usize {
    let usable = nofile.saturating_sub(NOFILE_RESERVE) / 2;
    (usable.min(MAX_HANDLES as u64) as usize).max(64)
}

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
    /// Its slot, charged to the guest process that caused it.
    slot: Charge,
}

/// The largest handle issued: a descriptor field read as an i32 must not
/// see it negative.
const MAX_ISSUED: u32 = i32::MAX as u32;

pub struct HandleTable {
    /// Where the search for the next free value starts.
    next: u32,
    table: HashMap<u32, Entry>,
    /// The table's slots, and each guest process's share of them: one for
    /// every handle, and one for every descriptor still closing.
    slots: Pool,
    /// Descriptors let go of here and handed to the closer, not yet closed.
    closing: Closing,
}

/// Descriptors the table let go of that the closer (closer.rs) has not yet
/// closed: still open on the host, so still counted against the table and
/// their process's share. Refunded as a handle was removed, they were
/// counted nowhere, and a guest process opening and closing display files
/// while the closer waited on a modeset queued them without bound, until
/// the backend itself ran out of descriptors -- for every process of the VM
/// (review 2026-09-26, backend 14).
///
/// Counts only, never refused from: what is closing already holds a slot.
struct Closing {
    all: Pool,
    /// Of those, modeset files, which count against the VM's and each
    /// process's NVKMS opens too (review 2026-09-29 1.12).
    modesets: Pool,
}

impl Closing {
    fn new() -> Self {
        let count = || Pool::new(u64::MAX, Share::custom(u64::MAX, 0, 0));
        Closing {
            all: count(),
            modesets: count(),
        }
    }
}

/// A descriptor on its way to the closer, still counted: `item` is dropped
/// (closed) first, then its slot goes, then the closing counts.
pub struct Closed<T> {
    item: Option<T>,
    _slot: Charge,
    _closing: Charge,
    _modeset: Option<Charge>,
}

impl<T> Drop for Closed<T> {
    fn drop(&mut self) {
        drop(self.item.take());
    }
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
            slots: Pool::new(limit as u64, Self::share_of(limit)),
            closing: Closing::new(),
        }
    }

    fn share_of(limit: usize) -> Share {
        Share::quarter(limit as u64, 16)
    }

    /// Change the limit (from the backend's RLIMIT_NOFILE at start). Handles
    /// already issued stay; a lower limit refuses new ones until enough go.
    pub fn set_limit(&mut self, limit: usize) {
        self.slots.resize(limit as u64, Self::share_of(limit));
    }

    pub fn limit(&self) -> usize {
        self.slots.size() as usize
    }

    /// Take ownership of `fd` and issue a handle for it, charged to no
    /// process in particular (only the table's own limit applies).
    pub fn insert(&mut self, fd: OwnedFd, kind: HandleKind) -> std::result::Result<u32, TableFull> {
        self.insert_for(fd, kind, Owner::Unknown)
    }

    /// Take ownership of `fd` and issue a handle for it, charged to `owner`.
    ///
    /// When the table is full, or `owner` holds its share of it, the
    /// descriptor is dropped, i.e. closed: there is nobody left to hand it
    /// to, and a descriptor that nobody can name is a leak.
    pub fn insert_for(
        &mut self,
        fd: OwnedFd,
        kind: HandleKind,
        owner: Owner,
    ) -> std::result::Result<u32, TableFull> {
        let slot = match self.slots.try_take(owner, 1) {
            Ok(slot) => slot,
            Err(why) => {
                let (closing, closing_total) =
                    (self.closing.all.held(owner), self.closing.all.in_use());
                if why != Over::Pool {
                    log::warn!(
                        "handle table: guest process {owner:?} holds {} of {} handles, \
                         {closing} more still closing ({why:?}); refusing a {kind:?}",
                        self.slots.held(owner).saturating_sub(closing),
                        self.limit()
                    );
                } else if closing_total > 0 {
                    log::warn!(
                        "handle table: full, {closing_total} of it still closing; refusing a \
                         {kind:?}"
                    );
                }
                return Err(TableFull);
            }
        };
        // Handles are issued in [1, i32::MAX]. The guest's structs carry a
        // handle in a descriptor field, where RM, UVM and the guest module
        // read it as a signed int: a handle past i32::MAX reads as a
        // negative descriptor there, and after 2^31 opens and closes every
        // new file of the VM failed its event, fd and UVM registrations
        // (review 2026-09-29 1.2).
        //
        // Terminates: at most MAX_HANDLES values are live, far fewer than
        // i32::MAX, so some candidate is free, and in practice the first
        // one almost always is.
        loop {
            let h = self.next;
            self.next = if h >= MAX_ISSUED { 1 } else { h + 1 };
            if h == 0 || h > MAX_ISSUED || self.table.contains_key(&h) {
                continue;
            }
            self.table.insert(
                h,
                Entry {
                    fd,
                    kind,
                    buried: false,
                    slot,
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

    /// The guest process `handle` is charged to; `Unknown` for none, or no
    /// such handle.
    pub fn owner(&self, handle: u32) -> Owner {
        self.table
            .get(&handle)
            .map_or(Owner::Unknown, |e| e.slot.owner())
    }

    /// Handles `owner` holds, not counting its descriptors still closing
    /// (none are charged to an unknown owner).
    #[cfg(test)]
    pub fn held_by(&self, owner: Owner) -> u64 {
        if owner == Owner::Unknown {
            return 0;
        }
        self.table
            .values()
            .filter(|e| e.slot.owner() == owner)
            .count() as u64
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

    /// `item` -- the descriptor of a handle of `kind` just removed,
    /// charged to `owner`, on its way to the closer -- counted against the
    /// table and `owner`'s share until it is dropped.
    pub fn closing<T>(&self, item: T, owner: Owner, kind: HandleKind) -> Closed<T> {
        let modeset = kind == HandleKind::Dev(protocol::messages::DeviceKind::Modeset);
        Closed {
            item: Some(item),
            _slot: self.slots.hold(owner, 1),
            _closing: self.closing.all.hold(owner, 1),
            _modeset: modeset.then(|| self.closing.modesets.hold(owner, 1)),
        }
    }

    /// Modeset files still closing: the VM's, and `owner`'s.
    pub fn closing_modesets(&self, owner: Owner) -> (u64, u64) {
        let m = &self.closing.modesets;
        (m.in_use(), m.held(owner))
    }

    /// Remove `handle`, returning its descriptor (which closes when dropped).
    pub fn remove(&mut self, handle: u32) -> Result<(OwnedFd, HandleKind)> {
        let e = self
            .table
            .remove(&handle)
            .ok_or(DeviceError::BadHandle(handle as u64))?;
        // Its slot goes with the entry.
        Ok((e.fd, e.kind))
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

    fn make_fd() -> OwnedFd {
        crate::sys::fd::open(c"/dev/null", libc::O_RDONLY | libc::O_CLOEXEC).unwrap()
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
        t.next = i32::MAX as u32 - 1;
        assert_eq!(t.insert(make_fd(), CTL).unwrap(), i32::MAX as u32 - 1);
        assert_eq!(t.insert(make_fd(), CTL).unwrap(), i32::MAX as u32);
        // Nothing past i32::MAX, 0 is never issued, and 1 is live: the next
        // one is 2.
        assert_eq!(t.insert(make_fd(), CTL).unwrap(), 2);
        assert!(t.kind(0).is_none() && t.kind(u32::MAX).is_none());
    }

    /// A handle is never negative as an i32: the guest's structs carry it
    /// in descriptor fields read as signed ints (review 2026-09-29 1.2).
    #[test]
    fn no_handle_reads_as_a_negative_descriptor() {
        let mut t = HandleTable::new();
        for start in [0x8000_0000, u32::MAX - 1, u32::MAX, 0xC000_0000] {
            t.next = start;
            let h = t.insert(make_fd(), CTL).unwrap();
            assert!((h as i32) > 0, "{start:#x} issued {h:#x}");
        }
    }

    #[test]
    fn a_full_table_refuses_with_emfile_and_closes_the_descriptor() {
        let mut t = HandleTable::with_limit(2);
        t.insert(make_fd(), CTL).unwrap();
        t.insert(make_fd(), CTL).unwrap();
        // The refused descriptor is the write end of a pipe whose read end
        // this test keeps: the read end sees EOF exactly when every write
        // end is closed. Asking whether the number is still open instead
        // raced with other test threads reusing it.
        let (read, write) = crate::sys::fd::pipe2(libc::O_CLOEXEC | libc::O_NONBLOCK).unwrap();
        let err = t.insert(write, CTL).unwrap_err();
        assert_eq!(err.errno(), libc::EMFILE);
        // The refused descriptor was closed, not leaked.
        let mut b = [0u8; 1];
        let n = crate::sys::fd::read(&read, &mut b);
        assert!(
            matches!(n, Ok(0)) || crate::testfd::only_end_here(read.as_fd()),
            "EOF: no write end is left open"
        );
        // Space comes back when a handle goes.
        let h = t.handles()[0];
        t.remove(h).unwrap();
        assert!(t.insert(make_fd(), CTL).is_ok());
    }

    #[test]
    fn one_guest_process_cannot_take_the_whole_table() {
        use crate::quota::Owner;
        let a = Owner::Proc {
            tgid: 10,
            start_ns: 1,
        };
        let b = Owner::Proc {
            tgid: 11,
            start_ns: 2,
        };
        let mut t = HandleTable::with_limit(256);
        let mut mine = Vec::new();
        loop {
            match t.insert_for(make_fd(), CTL, a) {
                Ok(h) => mine.push(h),
                Err(e) => {
                    assert_eq!(e.errno(), libc::EMFILE);
                    break;
                }
            }
        }
        assert_eq!(mine.len(), 64, "a quarter of the table");
        assert_eq!(t.held_by(a), 64);
        assert_eq!(t.owner(mine[0]), a);
        // Another process still opens.
        let hb = t.insert_for(make_fd(), CTL, b).unwrap();
        assert_eq!(t.owner(hb), b);
        // What a process closes it may open again.
        t.remove(mine.pop().unwrap()).unwrap();
        assert_eq!(t.held_by(a), 63);
        assert!(t.insert_for(make_fd(), CTL, a).is_ok());
        // A guest that does not say is held to the table alone.
        assert!(t.insert(make_fd(), CTL).is_ok());
        assert_eq!(
            t.owner(t.handles().into_iter().max().unwrap()),
            Owner::Unknown
        );
        t.drain_all();
        assert_eq!(t.held_by(a), 0);
    }

    #[test]
    fn the_limit_follows_the_descriptors_the_backend_may_open() {
        assert_eq!(limit_for_nofile(1024), 64);
        assert_eq!(limit_for_nofile(4096), 1536);
        assert_eq!(limit_for_nofile(524_288), MAX_HANDLES);
        let mut t = HandleTable::with_limit(4);
        t.set_limit(limit_for_nofile(4096));
        assert_eq!(t.limit(), 1536);
    }

    #[test]
    fn a_duplicate_outlives_the_handle_it_came_from() {
        let mut t = HandleTable::new();
        let h = t.insert(make_fd(), CTL).unwrap();
        let (dup, kind) = t.dup(h).unwrap();
        assert_eq!(kind, CTL);
        t.remove(h).unwrap();
        assert!(crate::sys::fd::is_open(dup.as_raw_fd()));
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
