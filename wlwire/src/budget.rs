// SPDX-License-Identifier: Apache-2.0
//! Budgets a charge is taken from all together, or not at all.
//!
//! What an engine holds is charged to more than one budget at once: its own
//! connection's limits, and those its owner shares out among connections --
//! the VM's, which the backend gives every connection of a VM, and a guest
//! process's share of it. A charge that any of them refuses is taken from
//! none: the ones before it give back what they took. What is given back
//! goes to every one. [`Budgets`] holds that rule once, for shm pools and
//! the memory they and blobs cover ([`ShmCharge`]) and for what stream
//! sinks hold ([`ByteBudget`]).

#![forbid(unsafe_code)]

use std::sync::Arc;

use crate::shm::ShmCharge;
use crate::stream::ByteBudget;

/// Budgets charged together, in the order they were added.
pub struct Budgets<B: ?Sized>(Vec<Arc<B>>);

impl<B: ?Sized> Default for Budgets<B> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<B: ?Sized> Clone for Budgets<B> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<B: ?Sized> Budgets<B> {
    /// Charge `b` as well, after the ones already here.
    pub fn push(&mut self, b: Arc<B>) {
        self.0.push(b);
    }

    /// Charge `more`'s budgets as well, after these.
    pub fn extend(&mut self, more: &Budgets<B>) {
        self.0.extend(more.0.iter().cloned());
    }

    /// `take` from every budget in order; if one refuses, `give` back to
    /// the ones before it, and nothing is taken.
    fn all_or_none(&self, take: impl Fn(&B) -> bool, give: impl Fn(&B)) -> bool {
        for (i, b) in self.0.iter().enumerate() {
            if !take(b) {
                for done in &self.0[..i] {
                    give(done);
                }
                return false;
            }
        }
        true
    }

    fn each(&self, give: impl Fn(&B)) {
        for b in &self.0 {
            give(b);
        }
    }
}

impl Budgets<dyn ShmCharge> {
    /// `bytes` and `pools` from every budget, or from none.
    pub fn take(&self, bytes: u64, pools: u64) -> bool {
        self.all_or_none(|b| b.take(bytes, pools), |b| b.give(bytes, pools))
    }

    /// `bytes` and `pools`, taken before, back to every budget.
    pub fn give(&self, bytes: u64, pools: u64) {
        if bytes > 0 || pools > 0 {
            self.each(|b| b.give(bytes, pools));
        }
    }
}

impl Budgets<dyn ByteBudget> {
    /// `n` bytes from every budget, or from none.
    pub fn take(&self, n: usize) -> bool {
        self.all_or_none(|b| b.take(n), |b| b.give(n))
    }

    /// `n` bytes, taken before, back to every budget.
    pub fn give(&self, n: usize) {
        if n > 0 {
            self.each(|b| b.give(n));
        }
    }
}
