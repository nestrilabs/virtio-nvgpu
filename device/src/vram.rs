//! A guest's video memory, counted and held to `--vram-limit-mib`.
//!
//! Every RM call a guest makes is the backend's on the host, so RM sees one
//! process per guest with the whole card to allocate from. Nothing between
//! the guest and the card bounds what it may take, and video memory cannot be
//! reclaimed from a guest that holds it: one guest can leave every other on
//! the card, and the host's own desktop, with nothing.
//!
//! This is the bookkeeping, and only that. It knows nothing of RM's classes
//! or parameter blocks: the RM path says what was allocated, under which
//! parent, and how many bytes of video memory it holds, and asks here whether
//! an allocation fits. Refusing one is the RM path's job, and it refuses the
//! way RM refuses an allocation it cannot back -- `NV_ERR_NO_MEMORY` in the
//! caller's block, the ioctl itself succeeding -- so a guest over its limit
//! fails exactly as a guest on a full card does. That is the same choice the
//! DRM native-context path made in nesbox (`virtio-devices/src/gpu/vram.rs`):
//! no failure mode the guest's driver has not already met.
//!
//! # What a charge lives as long as
//!
//! RM frees an object together with everything allocated under it, and a
//! client together with everything in it. So every object the guest makes is
//! recorded with its parent, whether it holds memory or not: freeing a device
//! has to release the memory allocated under its subdevice, and that cannot be
//! found from the memory object alone. A duplicate (`RM_DUP_OBJECT`) is the
//! same memory under a second handle, charged once and released with the
//! last handle that holds it.
//!
//! # Admitted by what was asked, charged by what RM took
//!
//! RM rounds an allocation up to its page size and says so in the reply. The
//! request is admitted against the size asked for, before the host sees it;
//! the charge is the size RM reports after. The limit can therefore be passed
//! by at most one allocation's rounding, and is never passed by more.
//!
//! # Without a limit
//!
//! Everything is still counted, so a teardown line can say what a workload
//! peaked at: the number a limit has to be chosen from.

use std::collections::HashMap;

/// An RM client handle (`hClient` / `hRoot`).
pub type Client = u32;
/// An RM object handle, unique within its client.
pub type Handle = u32;

pub struct Vram {
    limit: Option<u64>,
    objects: HashMap<(Client, Handle), Object>,
    memory: HashMap<u64, Memory>,
    next_memory: u64,
    /// Which clients were made on which open file, so closing the file
    /// releases them as RM does.
    files: HashMap<u64, Vec<Client>>,
    in_use: u64,
    peak: u64,
    refused: u64,
}

struct Object {
    parent: Handle,
    memory: Option<u64>,
}

struct Memory {
    bytes: u64,
    /// Handles that hold it: the one it was made under and every duplicate.
    holders: u32,
}

/// One MiB, the unit the limit is given in.
const MIB: u64 = 1 << 20;

impl Vram {
    /// `None` counts without limiting.
    pub fn new(limit_mib: Option<u64>) -> Self {
        Self {
            limit: limit_mib.map(|m| m.saturating_mul(MIB)),
            objects: HashMap::new(),
            memory: HashMap::new(),
            next_memory: 1,
            files: HashMap::new(),
            in_use: 0,
            peak: 0,
            refused: 0,
        }
    }

    /// The limit, in bytes.
    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    /// The limit as the device config announces it: MiB, 0 for none.
    pub fn limit_mib(&self) -> u64 {
        self.limit.map_or(0, |b| b / MIB)
    }

    /// Video memory the guest holds now, in bytes.
    pub fn in_use(&self) -> u64 {
        self.in_use
    }

    /// The most it has held at once.
    pub fn peak(&self) -> u64 {
        self.peak
    }

    /// Allocations refused for want of room.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// What is left under the limit; `None` without one. For answers that
    /// tell the guest how much video memory it has.
    pub fn remaining(&self) -> Option<u64> {
        self.limit.map(|l| l.saturating_sub(self.in_use))
    }

    /// Whether an allocation of `bytes` fits. Asked before the host sees it;
    /// a `false` is counted as a refusal.
    pub fn admit(&mut self, bytes: u64) -> bool {
        let fits = match self.limit {
            None => true,
            Some(l) => self.in_use.checked_add(bytes).is_some_and(|t| t <= l),
        };
        if !fits {
            self.refused += 1;
        }
        fits
    }

    /// The host made `handle` under `parent` in `client`, holding `bytes` of
    /// video memory (`None` for an object that holds none). Called only after
    /// the host succeeded.
    pub fn allocated(
        &mut self,
        client: Client,
        parent: Handle,
        handle: Handle,
        bytes: Option<u64>,
    ) {
        // RM does not hand out a live handle twice. If it appears to, the old
        // record is stale -- a free this side never saw -- and is dropped
        // rather than leaked.
        self.drop_object(client, handle);
        let memory = bytes.map(|b| {
            let id = self.next_memory;
            self.next_memory += 1;
            self.memory.insert(
                id,
                Memory {
                    bytes: b,
                    holders: 1,
                },
            );
            self.in_use = self.in_use.saturating_add(b);
            self.peak = self.peak.max(self.in_use);
            id
        });
        self.objects
            .insert((client, handle), Object { parent, memory });
    }

    /// The host made `handle` in `client` a duplicate of `src_handle` in
    /// `src_client`. It holds the same memory, if any, and costs nothing.
    pub fn duplicated(
        &mut self,
        client: Client,
        parent: Handle,
        handle: Handle,
        src_client: Client,
        src_handle: Handle,
    ) {
        let memory = self
            .objects
            .get(&(src_client, src_handle))
            .and_then(|o| o.memory);
        // Held before the old record at `handle` is dropped: if that record
        // was the source itself, dropping it first would free the memory the
        // duplicate is about to hold.
        if let Some(m) = memory.and_then(|id| self.memory.get_mut(&id)) {
            m.holders += 1;
        }
        self.drop_object(client, handle);
        self.objects
            .insert((client, handle), Object { parent, memory });
    }

    /// The host freed `handle` in `client`, and with it everything under it.
    /// Freeing the client's own handle frees the whole client.
    pub fn freed(&mut self, client: Client, handle: Handle) {
        if handle == client {
            let all: Vec<Handle> = self
                .objects
                .keys()
                .filter(|(c, _)| *c == client)
                .map(|(_, h)| *h)
                .collect();
            for h in all {
                self.drop_object(client, h);
            }
            return;
        }
        let mut children: HashMap<Handle, Vec<Handle>> = HashMap::new();
        for ((c, h), o) in &self.objects {
            if *c == client && *h != o.parent {
                children.entry(o.parent).or_default().push(*h);
            }
        }
        let mut doomed = vec![handle];
        let mut i = 0;
        while i < doomed.len() {
            if let Some(kids) = children.remove(&doomed[i]) {
                doomed.extend(kids);
            }
            i += 1;
        }
        for h in doomed {
            self.drop_object(client, h);
        }
    }

    /// `client` was made on the open file `file`.
    pub fn client_opened(&mut self, file: u64, client: Client) {
        let v = self.files.entry(file).or_default();
        if !v.contains(&client) {
            v.push(client);
        }
    }

    /// The open file `file` was closed: RM frees every client made on it.
    pub fn file_closed(&mut self, file: u64) {
        for client in self.files.remove(&file).unwrap_or_default() {
            self.freed(client, client);
        }
    }

    fn drop_object(&mut self, client: Client, handle: Handle) {
        let Some(o) = self.objects.remove(&(client, handle)) else {
            return;
        };
        let Some(id) = o.memory else { return };
        let Some(m) = self.memory.get_mut(&id) else {
            return;
        };
        m.holders -= 1;
        if m.holders == 0 {
            self.in_use = self.in_use.saturating_sub(m.bytes);
            self.memory.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Client = 0xc1d0_0001;
    const DEV: Handle = 0xcaf0_0001;
    const SUB: Handle = 0xcaf0_0002;

    /// A client with a device and a subdevice, as every RM user makes first.
    fn with_device(limit_mib: Option<u64>) -> Vram {
        let mut v = Vram::new(limit_mib);
        v.allocated(C, C, C, None);
        v.allocated(C, C, DEV, None);
        v.allocated(C, DEV, SUB, None);
        v
    }

    #[test]
    fn without_a_limit_everything_fits_and_is_still_counted() {
        let mut v = with_device(None);
        assert!(v.admit(u64::MAX));
        v.allocated(C, SUB, 0x10, Some(64 * MIB));
        assert_eq!(v.in_use(), 64 * MIB);
        assert_eq!(v.limit_mib(), 0);
        assert_eq!(v.remaining(), None);
    }

    #[test]
    fn an_allocation_past_the_limit_is_refused_and_counted() {
        let mut v = with_device(Some(100));
        assert!(v.admit(64 * MIB));
        v.allocated(C, SUB, 0x10, Some(64 * MIB));
        assert!(!v.admit(64 * MIB));
        assert_eq!(v.refused(), 1);
        assert!(v.admit(36 * MIB), "exactly up to the limit fits");
        assert_eq!(v.remaining(), Some(36 * MIB));
    }

    #[test]
    fn a_size_that_would_overflow_is_refused_not_wrapped() {
        let mut v = with_device(Some(100));
        v.allocated(C, SUB, 0x10, Some(MIB));
        assert!(!v.admit(u64::MAX));
    }

    #[test]
    fn the_charge_is_what_rm_took_not_what_was_asked() {
        let mut v = with_device(Some(100));
        assert!(v.admit(1));
        v.allocated(C, SUB, 0x10, Some(2 * MIB));
        assert_eq!(v.in_use(), 2 * MIB);
    }

    #[test]
    fn freeing_memory_gives_it_back() {
        let mut v = with_device(Some(100));
        v.allocated(C, SUB, 0x10, Some(64 * MIB));
        v.freed(C, 0x10);
        assert_eq!(v.in_use(), 0);
        assert_eq!(v.peak(), 64 * MIB);
    }

    /// RM frees what is under an object with it. Memory made under the
    /// subdevice goes when the device above it is freed.
    #[test]
    fn freeing_an_ancestor_frees_the_memory_under_it() {
        let mut v = with_device(Some(100));
        v.allocated(C, SUB, 0x10, Some(32 * MIB));
        v.allocated(C, DEV, 0x11, Some(16 * MIB));
        v.freed(C, DEV);
        assert_eq!(v.in_use(), 0);
    }

    #[test]
    fn freeing_one_branch_leaves_its_sibling() {
        let mut v = with_device(Some(100));
        v.allocated(C, SUB, 0x10, Some(32 * MIB));
        v.allocated(C, DEV, 0x11, Some(16 * MIB));
        v.freed(C, SUB);
        assert_eq!(v.in_use(), 16 * MIB);
    }

    #[test]
    fn freeing_the_client_frees_everything_in_it() {
        let mut v = with_device(Some(100));
        v.allocated(C, SUB, 0x10, Some(32 * MIB));
        v.freed(C, C);
        assert_eq!(v.in_use(), 0);
    }

    #[test]
    fn another_clients_memory_is_untouched() {
        let other: Client = 0xc1d0_0002;
        let mut v = with_device(Some(100));
        v.allocated(other, other, other, None);
        v.allocated(other, other, DEV, None);
        v.allocated(other, DEV, 0x10, Some(8 * MIB));
        v.allocated(C, SUB, 0x10, Some(32 * MIB));
        v.freed(C, DEV);
        assert_eq!(v.in_use(), 8 * MIB, "same handles, different client");
    }

    #[test]
    fn closing_the_file_frees_its_clients() {
        let mut v = with_device(Some(100));
        v.client_opened(7, C);
        v.allocated(C, SUB, 0x10, Some(32 * MIB));
        v.file_closed(7);
        assert_eq!(v.in_use(), 0);
    }

    /// A duplicate is the same memory: charged once, and released only when
    /// the last handle to it goes.
    #[test]
    fn a_duplicate_costs_nothing_and_keeps_the_memory_alive() {
        let other: Client = 0xc1d0_0002;
        let mut v = with_device(Some(100));
        v.allocated(other, other, other, None);
        v.allocated(C, SUB, 0x10, Some(32 * MIB));
        v.duplicated(other, other, 0x20, C, 0x10);
        assert_eq!(v.in_use(), 32 * MIB);
        v.freed(C, 0x10);
        assert_eq!(v.in_use(), 32 * MIB, "the duplicate still holds it");
        v.freed(other, other);
        assert_eq!(v.in_use(), 0);
    }

    #[test]
    fn a_duplicate_onto_its_own_handle_keeps_the_memory() {
        let mut v = with_device(Some(100));
        v.allocated(C, SUB, 0x10, Some(32 * MIB));
        v.duplicated(C, SUB, 0x10, C, 0x10);
        assert_eq!(v.in_use(), 32 * MIB);
        v.freed(C, 0x10);
        assert_eq!(v.in_use(), 0);
    }

    #[test]
    fn a_reused_handle_does_not_leak_the_old_charge() {
        let mut v = with_device(Some(100));
        v.allocated(C, SUB, 0x10, Some(32 * MIB));
        v.allocated(C, SUB, 0x10, Some(8 * MIB));
        assert_eq!(v.in_use(), 8 * MIB);
    }

    #[test]
    fn freeing_what_was_never_seen_is_harmless() {
        let mut v = with_device(Some(100));
        v.freed(C, 0xdead);
        v.freed(0xbad, 0xbad);
        v.file_closed(99);
        assert_eq!(v.in_use(), 0);
    }
}
