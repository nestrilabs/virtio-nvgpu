// SPDX-License-Identifier: Apache-2.0
//! Guest processes' shares of the VM-wide budgets.
//!
//! One backend serves every process of a guest, and most of what it holds
//! for them comes out of pools sized per VM: host descriptors (the handle
//! table), the shared window's zones, semaphore-surface fence contexts,
//! NVKMS opens, UVM aperture placements. With a cap only per VM, one guest
//! process could take a whole pool and leave every other process of the VM
//! with ENOMEM or EMFILE -- a starvation that has no native counterpart,
//! where each process's descriptors, mappings and BAR1 space are its own.
//!
//! So each pool is also split among the guest processes that draw on it. A
//! process is an [`Owner`]: the one the guest kernel says made the call that
//! opened the file the resource belongs to ([`ProcId`], sent on RM calls,
//! OPEN and HOST_OP by a guest offered BCAP_PROC_ID). What any one owner may
//! hold is bounded by a [`Share`] of the pool, and the last part of the pool
//! is kept for owners that hold little of it, so a process that has taken its
//! whole share cannot take the first resources of the next one either. The
//! per-VM cap stays the outer bound.
//!
//! What this does not do: tell apart processes the guest kernel does not. A
//! process can fork, and each child is a new owner with a share of its own,
//! so a guest process that forks enough can still take a pool; it takes
//! [`Share::owners_to_exhaust`] of them, and the guest's own process limits
//! (RLIMIT_NPROC, a pids cgroup, a sandbox's) are what bound that. A guest
//! that does not say which process makes a call ([`Owner::Unknown`]) is held
//! to the per-VM caps alone, as before.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use protocol::messages::ProcId;

/// Who a resource is charged to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Owner {
    /// The guest did not say (no BCAP_PROC_ID, or a message that carries no
    /// process): only the per-VM caps apply.
    #[default]
    Unknown,
    /// One guest process for the guest's lifetime (see [`ProcId`]).
    Proc { tgid: u32, start_ns: u64 },
}

impl Owner {
    /// The process a wire [`ProcId`] names. The euid is not part of it:
    /// shares are per process.
    pub fn from_wire(p: &ProcId) -> Self {
        Owner::Proc {
            tgid: p.tgid,
            start_ns: p.start_ns,
        }
    }

    /// The process a [`ProcId`] trailer at `at` of `payload` names, when
    /// the session carries them and the payload holds one.
    pub fn from_trailer(payload: &[u8], at: usize, proc_ids: bool) -> Self {
        if !proc_ids {
            return Owner::Unknown;
        }
        let Some(b) = payload.get(at..at + size_of::<ProcId>()) else {
            return Owner::Unknown;
        };
        let id = ProcId {
            start_ns: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            tgid: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            euid: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        };
        Owner::from_wire(&id)
    }
}

/// How a pool of `size` units is split among owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
    /// The most one owner may hold.
    pub per_owner: u64,
    /// The last units of the pool, which only an owner holding at most
    /// `floor` (this request included) may take.
    pub reserve: u64,
    pub floor: u64,
}

impl Share {
    /// The split every pool here uses unless it says otherwise: an owner
    /// takes at most a quarter of the pool, and the last sixteenth is kept
    /// for owners holding at most a sixty-fourth of it (at least `min_floor`,
    /// so a small pool's first resources are never all reserved away).
    pub const fn quarter(size: u64, min_floor: u64) -> Self {
        let floor = if size / 64 > min_floor {
            size / 64
        } else {
            min_floor
        };
        Share {
            per_owner: size / 4,
            reserve: size / 16,
            floor,
        }
    }

    /// A looser split, for pools one process may legitimately need much of
    /// (the shared window: one game maps hundreds of MiB of video memory):
    /// half the pool per owner, the last eighth kept for owners holding at
    /// most a sixteenth.
    pub const fn half(size: u64) -> Self {
        Share {
            per_owner: size / 2,
            reserve: size / 8,
            floor: size / 16,
        }
    }

    /// `p` percent of the pool per owner, for a pool an operator has said
    /// one process may need most of (the window, `--window-owner-share`):
    /// the last eighth kept for owners holding at most a sixteenth, as in
    /// [`Share::half`], except that the reserve never takes from what one
    /// owner was given -- past 87.5 % it shrinks to what is left beside a
    /// whole share, and the floor with it. At 50 this is `half` exactly.
    ///
    /// What a larger share gives up is in [`Share::owners_to_exhaust`]: 2
    /// owners for 50 up to 87.5 %, 1 from there. Then one process can take
    /// the pool down to its reserve, and every other process of the VM has
    /// only the reserve, each at most the floor of it.
    pub const fn percent(size: u64, p: u8) -> Self {
        let per_owner = (size as u128 * p as u128 / 100) as u64;
        let rest = size - if per_owner > size { size } else { per_owner };
        let reserve = if size / 8 < rest { size / 8 } else { rest };
        let floor = if size / 16 < reserve {
            size / 16
        } else {
            reserve
        };
        Share {
            per_owner,
            reserve,
            floor,
        }
    }

    /// Owners each holding a full share it takes to leave the pool with
    /// only its reserve.
    pub fn owners_to_exhaust(&self, size: u64) -> u64 {
        (size - self.reserve).div_ceil(self.per_owner.max(1))
    }
}

/// Why a request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Over {
    /// The pool itself is full.
    Pool,
    /// The owner holds its whole share.
    Owner,
    /// Only the reserve is left, and the owner holds more than the floor.
    Reserve,
}

/// May `o`, which holds `held`, take `n` more units of a pool of `size`,
/// `in_use` of which are taken? For pools that count what each owner holds
/// themselves.
pub fn admits(
    share: &Share,
    o: Owner,
    held: u64,
    n: u64,
    in_use: u64,
    size: u64,
) -> Result<(), Over> {
    let after = in_use.saturating_add(n);
    if after > size {
        return Err(Over::Pool);
    }
    if o == Owner::Unknown {
        return Ok(());
    }
    let mine = held.saturating_add(n);
    if mine > share.per_owner {
        return Err(Over::Owner);
    }
    if after > size.saturating_sub(share.reserve) && mine > share.floor {
        return Err(Over::Reserve);
    }
    Ok(())
}

/// What each owner holds of one pool.
#[derive(Debug, Default)]
pub struct Ledger {
    held: HashMap<Owner, u64>,
}

impl Ledger {
    pub fn held(&self, o: Owner) -> u64 {
        self.held.get(&o).copied().unwrap_or(0)
    }

    /// May `o` take `n` more units of a pool of `size`, `in_use` of which
    /// are taken?
    pub fn admits(
        &self,
        share: &Share,
        o: Owner,
        n: u64,
        in_use: u64,
        size: u64,
    ) -> Result<(), Over> {
        admits(share, o, self.held(o), n, in_use, size)
    }

    pub fn charge(&mut self, o: Owner, n: u64) {
        if o != Owner::Unknown && n > 0 {
            *self.held.entry(o).or_insert(0) += n;
        }
    }

    pub fn refund(&mut self, o: Owner, n: u64) {
        if let Some(h) = self.held.get_mut(&o) {
            *h = h.saturating_sub(n);
            if *h == 0 {
                self.held.remove(&o);
            }
        }
    }

    pub fn clear(&mut self) {
        self.held.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(tgid: u32) -> Owner {
        Owner::Proc {
            tgid,
            start_ns: u64::from(tgid) * 1000,
        }
    }

    #[test]
    fn one_owner_takes_at_most_its_share_and_the_pool_bounds_everyone() {
        let s = Share::quarter(64, 1);
        assert_eq!((s.per_owner, s.reserve, s.floor), (16, 4, 1));
        let mut l = Ledger::default();
        let mut used = 0;
        for _ in 0..16 {
            l.admits(&s, p(1), 1, used, 64).unwrap();
            l.charge(p(1), 1);
            used += 1;
        }
        assert_eq!(l.admits(&s, p(1), 1, used, 64), Err(Over::Owner));
        // Another process is not affected.
        assert_eq!(l.admits(&s, p(2), 1, used, 64), Ok(()));
        // Unknown owners see only the pool.
        assert_eq!(l.admits(&s, Owner::Unknown, 48, used, 64), Ok(()));
        assert_eq!(l.admits(&s, Owner::Unknown, 49, used, 64), Err(Over::Pool));
        l.refund(p(1), 16);
        assert_eq!(l.held(p(1)), 0);
    }

    #[test]
    fn the_reserve_is_kept_for_owners_that_hold_little() {
        let s = Share::quarter(64, 1);
        let mut l = Ledger::default();
        // Three owners with full shares: 48 of 64 used, the fourth takes
        // what is left above the reserve.
        for o in 1..=3 {
            l.charge(p(o), 16);
        }
        l.charge(p(4), 12);
        let used = 60;
        // The reserve (last 4) is closed to any of them...
        for o in 1..=4 {
            assert!(l.admits(&s, p(o), 1, used, 64).is_err(), "owner {o}");
        }
        // ...and open to a process holding nothing yet, up to the floor.
        assert_eq!(l.admits(&s, p(5), 1, used, 64), Ok(()));
        l.charge(p(5), 1);
        assert_eq!(l.admits(&s, p(5), 1, used + 1, 64), Err(Over::Reserve));
        assert_eq!(s.owners_to_exhaust(64), 4);
    }

    /// `percent(size, 50)` is `half` exactly, whatever the size; above it
    /// the reserve gives way only past 87.5 %, and from there one owner
    /// can take the pool down to the reserve.
    #[test]
    fn a_percent_share_is_half_at_fifty_and_keeps_its_reserve_reachable() {
        let mib = 1u64 << 20;
        for size in [
            1,
            7,
            4096,
            768 * mib,
            12288 * mib,
            64 << 30,
            (1 << 36) + 4097,
        ] {
            assert_eq!(Share::percent(size, 50), Share::half(size), "size {size}");
        }
        let size = 12288 * mib;
        let owners = |p| Share::percent(size, p).owners_to_exhaust(size);
        assert_eq!(
            [50, 75, 87, 88, 90, 95].map(owners),
            [2, 2, 2, 1, 1, 1],
            "owners it takes to leave only the reserve"
        );
        let s = Share::percent(size, 90);
        assert_eq!(s.reserve, size - s.per_owner, "a whole share is reachable");
        assert_eq!(s.floor, size / 16);
        let s = Share::percent(size, 95);
        assert_eq!(
            (s.reserve, s.floor),
            (size - s.per_owner, size - s.per_owner)
        );
        // One owner at 90 % takes it to the reserve, and not past it...
        let s = Share::percent(size, 90);
        assert_eq!(admits(&s, p(1), 0, s.per_owner, 0, size), Ok(()));
        assert_eq!(
            admits(&s, p(1), s.per_owner, 4096, s.per_owner, size),
            Err(Over::Owner)
        );
        // ...and another still takes its first mappings out of the reserve.
        assert_eq!(admits(&s, p(2), 0, s.floor, s.per_owner, size), Ok(()));
        assert_eq!(
            admits(&s, p(2), s.floor, 4096, s.per_owner + s.floor, size),
            Err(Over::Reserve)
        );
    }

    #[test]
    fn a_trailer_names_its_process_only_when_the_session_carries_them() {
        let mut b = vec![0u8; 8];
        b.extend_from_slice(&7u64.to_le_bytes());
        b.extend_from_slice(&42u32.to_le_bytes());
        b.extend_from_slice(&1000u32.to_le_bytes());
        assert_eq!(
            Owner::from_trailer(&b, 8, true),
            Owner::Proc {
                tgid: 42,
                start_ns: 7
            }
        );
        assert_eq!(Owner::from_trailer(&b, 8, false), Owner::Unknown);
        assert_eq!(Owner::from_trailer(&b[..20], 8, true), Owner::Unknown);
    }
}
