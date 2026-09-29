// SPDX-License-Identifier: Apache-2.0
//! Counting keys a guest chooses, in bounded memory.
//!
//! The backend counts every RM class and control command a workload uses
//! (`nvidia/`, `rm_classes` and `rm_controls`): the instrument the RM
//! allowlist (gen/rmallow) was checked against. Both keys are u32s the
//! guest writes, so a map with an entry per distinct key is a map a guest
//! can grow by one entry per call, four billion times over, and print back
//! as one multi-gigabyte log line at teardown (S-17). Past [`MAX_KEYS`]
//! distinct keys the rest are counted together, and the report comes out a
//! bounded number of entries per line.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

/// Distinct keys kept one by one: RM's whole control and class namespace a
/// real driver stack touches is a small fraction of this, so every key a real
/// run names is still counted by itself.
pub const MAX_KEYS: usize = 4096;

/// Entries per report line.
const PER_LINE: usize = 128;

#[derive(Debug, Default)]
pub struct Tally {
    counts: BTreeMap<u32, u64>,
    /// Calls whose key arrived after the table was full.
    overflow: u64,
}

impl Tally {
    pub fn add(&mut self, key: u32) {
        if let Some(n) = self.counts.get_mut(&key) {
            *n += 1;
        } else if self.counts.len() < MAX_KEYS {
            self.counts.insert(key, 1);
        } else {
            self.overflow += 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty() && self.overflow == 0
    }

    /// Distinct keys kept.
    pub fn len(&self) -> usize {
        self.counts.len()
    }

    /// Calls not counted by key because the table was full.
    pub fn overflow(&self) -> u64 {
        self.overflow
    }

    pub fn get(&self, key: u32) -> Option<u64> {
        self.counts.get(&key).copied()
    }

    /// The report, `key=count` pairs formatted by `key`, at most
    /// [`PER_LINE`] to a line.
    pub fn lines(&self, key: impl Fn(u32) -> String) -> Vec<String> {
        let all: Vec<String> = self
            .counts
            .iter()
            .map(|(k, n)| format!("{}={n}", key(*k)))
            .collect();
        all.chunks(PER_LINE).map(|c| c.join(" ")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guest_naming_a_new_key_every_call_grows_nothing_past_the_cap() {
        let mut t = Tally::default();
        for k in 0..(MAX_KEYS as u32 + 1000) {
            t.add(k);
        }
        assert_eq!(t.len(), MAX_KEYS);
        assert_eq!(t.overflow(), 1000);
        // A key already kept is still counted by key once the table is full.
        t.add(7);
        assert_eq!(t.get(7), Some(2));
        assert_eq!(t.overflow(), 1000);
    }

    #[test]
    fn the_report_comes_out_a_bounded_number_of_entries_per_line() {
        let mut t = Tally::default();
        for k in 0..300 {
            t.add(k);
        }
        let lines = t.lines(|k| format!("{k:#x}"));
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.split(' ').count() <= PER_LINE));
        assert!(lines[0].starts_with("0x0=1 0x1=1"));
    }
}
