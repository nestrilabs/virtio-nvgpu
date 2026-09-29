// SPDX-License-Identifier: Apache-2.0
//! What the daemon holds for its clients, shared out among them.
//!
//! The daemon is one process serving every guest application, so what it
//! holds on their behalf -- descriptors, and bytes stream sinks keep for
//! readers that have not taken them -- comes out of one pool per daemon.
//! Counted only per connection, one application (or a few, or one that opens
//! many connections) could take the whole pool, and every other application
//! would find the daemon out of descriptors or memory: a starvation natively
//! bounded by the compositor's own limits, and there by each client's
//! descriptor limit, not by one shared process's.
//!
//! So each pool is split among owners -- the client process, as the socket's
//! peer credentials name it -- by a [`Share`]: what one owner may hold, and a
//! last part of the pool kept for owners that hold little, so an owner that
//! has taken its share cannot take the first resources of the next one
//! either. This is the backend's rule for what guest processes hold of a VM
//! (`device/src/quota.rs`), in the daemon, which cannot link the backend.
//!
//! What it does not do, as there: tell apart processes the kernel does not. A
//! process that forks takes [`Share::owners_to_exhaust`] children to leave
//! only the reserve, and the guest's process limits are what bound that.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Who something the daemon holds is charged to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Owner {
    /// A client process, by its pid in the daemon's namespace (SO_PEERCRED).
    Pid(i32),
    /// A connection whose peer the kernel cannot name (or export mode's,
    /// which are the host's clients): an owner of its own.
    Conn(u64),
}

/// How a pool of `size` units is split among owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
    /// The most one owner may hold.
    pub per_owner: u64,
    /// The last units of the pool, which only an owner holding at most
    /// `floor` (the request included) may take.
    pub reserve: u64,
    pub floor: u64,
}

impl Share {
    /// A quarter of the pool per owner; the last eighth kept for owners
    /// holding at most a sixty-fourth (and at least `min_floor`, so a small
    /// pool's first resources are never all reserved away).
    pub const fn quarter(size: u64, min_floor: u64) -> Self {
        let floor = if size / 64 > min_floor {
            size / 64
        } else {
            min_floor
        };
        Share {
            per_owner: size / 4,
            reserve: size / 8,
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

/// May an owner holding `held` take `n` more units of a pool of `size`,
/// `in_use` of which are taken? (`quota::admits`.)
pub fn admits(share: &Share, held: u64, n: u64, in_use: u64, size: u64) -> Result<(), Over> {
    let after = in_use.saturating_add(n);
    if after > size {
        return Err(Over::Pool);
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

/// A pool of bytes and what each owner holds of it, for what the owner's
/// connections take and give back as they go (stream sinks' pending data).
#[derive(Debug)]
pub struct Bytes {
    size: u64,
    share: Share,
    state: Mutex<(u64, HashMap<Owner, u64>)>,
}

impl Bytes {
    pub fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            size,
            share: Share::quarter(size, 64 << 10),
            state: Mutex::new((0, HashMap::new())),
        })
    }

    /// (bytes taken, bytes `o` holds).
    pub fn used(&self, o: Owner) -> (u64, u64) {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (s.0, s.1.get(&o).copied().unwrap_or(0))
    }

    fn take(&self, o: Owner, n: u64) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let held = s.1.get(&o).copied().unwrap_or(0);
        if admits(&self.share, held, n, s.0, self.size).is_err() {
            return false;
        }
        s.0 += n;
        *s.1.entry(o).or_insert(0) += n;
        true
    }

    fn give(&self, o: Owner, n: u64) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.0 = s.0.saturating_sub(n);
        if let Some(h) = s.1.get_mut(&o) {
            *h = h.saturating_sub(n);
            if *h == 0 {
                s.1.remove(&o);
            }
        }
    }

    /// `o`'s side of the pool, as an engine's stream sinks draw on it.
    pub fn for_owner(self: &Arc<Self>, o: Owner) -> Arc<dyn wlwire::stream::ByteBudget> {
        Arc::new(OwnerBytes {
            pool: self.clone(),
            owner: o,
        })
    }
}

struct OwnerBytes {
    pool: Arc<Bytes>,
    owner: Owner,
}

impl wlwire::stream::ByteBudget for OwnerBytes {
    fn take(&self, n: usize) -> bool {
        self.pool.take(self.owner, n as u64)
    }
    fn give(&self, n: usize) {
        self.pool.give(self.owner, n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_takes_its_share_and_the_reserve_is_kept_for_small_ones() {
        let s = Share::quarter(64, 1);
        assert_eq!((s.per_owner, s.reserve, s.floor), (16, 8, 1));
        assert_eq!(admits(&s, 16, 1, 16, 64), Err(Over::Owner));
        // Three full shares and a fourth up to the reserve...
        assert_eq!(admits(&s, 8, 1, 56, 64), Err(Over::Reserve));
        // ...which a newcomer may still take from, up to the floor.
        assert_eq!(admits(&s, 0, 1, 56, 64), Ok(()));
        assert_eq!(admits(&s, 1, 1, 57, 64), Err(Over::Reserve));
        assert_eq!(admits(&s, 0, 9, 56, 64), Err(Over::Pool));
        assert_eq!(s.owners_to_exhaust(64), 4);
    }

    #[test]
    fn bytes_are_charged_to_their_owner_and_given_back() {
        let b = Bytes::new(1 << 20);
        let (x, y) = (b.for_owner(Owner::Pid(1)), b.for_owner(Owner::Pid(2)));
        assert!(x.take(256 << 10));
        assert!(!x.take(1), "past a quarter");
        assert!(y.take(4096));
        assert_eq!(b.used(Owner::Pid(1)), ((256 << 10) + 4096, 256 << 10));
        x.give(256 << 10);
        y.give(4096);
        assert_eq!(b.used(Owner::Pid(2)), (0, 0));
    }
}
