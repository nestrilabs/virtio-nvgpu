// SPDX-License-Identifier: Apache-2.0
//! The UVM ranges a guest may map, and where in the UVM aperture they are.
//!
//! `cuCtxCreate` makes a UVM semaphore pool (UVM_ALLOC_SEMAPHORE_POOL, at a
//! base X the caller chose) and then maps the UVM file at X, offset X. UVM
//! takes that mapping only at the host address equal to the offset
//! (uvm.c:792), only for exactly the pool's range (uvm.c:872), and -- unless
//! the VA space is in multi-process sharing mode, which the backend forces
//! (guestptr.rs) -- only from the process that initialised it (uvm.c:784).
//! The window cannot hold it: its host address is wherever the VMM reserved
//! the window. So the VMM maps the pool at X in its own address space and
//! gives that range a memory slot of its own, inside a second guest-physical
//! region, the UVM aperture (shared memory region 2). The guest maps its vma
//! from the aperture offset the backend picked.
//!
//! This is the backend's half of that, and the only authority on it:
//!
//! - **What may be mapped.** Only a pool this VM's own UVM file was seen to
//!   create (a successful ALLOC_SEMAPHORE_POOL on that handle), asked for
//!   exactly -- same base, same length, read-write -- on a VA space in
//!   sharing mode, at a base in [`HVA_MIN`, `HVA_MAX`), [4 GiB, 32 TiB).
//!   The band normally holds nothing of the VMM's: its executable and heap
//!   sit at two-thirds of the 47-bit space (85 TiB), and its mappings grow
//!   down from below the stack or, under the legacy layout
//!   (`vm.legacy_va_layout`), up from a third of it (42.7 TiB) -- which the
//!   64 TiB top this band once had would have reached. Not with an
//!   unlimited stack rlimit (or one above about 96 TiB): x86 then starts
//!   the mmap area near 21 TiB, inside the band, and nesbox's
//!   `MAP_FIXED_NOREPLACE` can collide with a mapping of its own -- a
//!   refusal, which says the address is taken (SECURITY.md §11, F3). crosvm
//!   reserves the band before it maps anything, so there it never does.
//! - **How much.** Placements per file and per VM, bytes per file and per
//!   VM, and recorded ranges per file and per VM, all bounded.
//! - **Where in the aperture.** First fit in [`CHUNK`] granules. An offset is
//!   reused only after its withdraw has completed, and a withdraw happens
//!   only when the guest's vmas of it are gone (the last MUNMAP, the file's
//!   close, a session reset) -- so a stale guest vma reads nothing, never
//!   another placement's pages.
//! - **Two pools at one address.** Every pool of the VM lands in the one VMM
//!   address space, so a second UVM file's pool at an address a live one
//!   covers cannot be mapped. It is refused as every other placement that
//!   cannot be made is, ENOMEM, not with an errno of its own (it was
//!   EEXIST); but a refusal where the caller's budgets have room still says
//!   the address is taken, so a guest process can still learn another's
//!   pool address by trying, and can squat on one. That is the one VMM
//!   address space, and not fixable here: UVM maps a pool only at the host
//!   address equal to its offset.
//! - **Per guest process** (quota.rs). A process holds at most 16 placements
//!   and 64 MiB of them, the per-file bounds, whatever number of files it
//!   has, and the VM's last 8 placements and 32 MiB are kept for processes
//!   holding at most 2 and 8 MiB (B5). The pools a process has made, mapped
//!   or not, are bounded the same way before UVM is asked to make one:
//!   a pool is host kernel memory from the moment it is made (F1).
//!
//! Pure bookkeeping: `nvidia.rs` makes the calls, observes UVM's replies, and
//! asks the VMM to place and withdraw.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::quota::{Owner, admits};

/// Aperture offsets are handed out in granules of this size.
pub const CHUNK: u64 = 2 << 20;
/// The most of an aperture the backend will use, whatever the guest says.
pub const APERTURE_MAX: u64 = 1 << 30;
/// The largest one pool that is ever mapped.
pub const MAX_LEN: u64 = 64 << 20;
/// The band a pool's host address must lie in: [4 GiB, 32 TiB), the one the
/// VMM and the guest driver hold it to too (protocol's UVM_HVA_MIN/MAX).
pub const HVA_MIN: u64 = protocol::messages::UVM_HVA_MIN;
pub const HVA_MAX: u64 = protocol::messages::UVM_HVA_MAX;
/// Live placements, and their bytes, per VM.
pub const MAPS_PER_VM: usize = 64;
pub const BYTES_PER_VM: u64 = 256 << 20;
/// Live placements, and their bytes, per UVM file.
pub const MAPS_PER_FILE: usize = 16;
pub const BYTES_PER_FILE: u64 = 64 << 20;
/// Recorded pools per UVM file and per VM.
pub const RANGES_PER_FILE: usize = 256;
pub const RANGES_PER_VM: usize = 4096;
/// Bytes of pools, mapped or not, per UVM file and per VM: each is host
/// kernel memory UVM allocates at ALLOC_SEMAPHORE_POOL (F1).
pub const POOL_BYTES_PER_FILE: u64 = 256 << 20;
pub const POOL_BYTES_PER_VM: u64 = 1 << 30;
/// A guest process's share of placements, of their bytes, of recorded
/// pools and of their bytes (quota.rs, B5).
pub const MAPS_SHARE: crate::quota::Share = crate::quota::Share {
    per_owner: MAPS_PER_FILE as u64,
    reserve: 8,
    floor: 2,
};
pub const BYTES_SHARE: crate::quota::Share = crate::quota::Share {
    per_owner: BYTES_PER_FILE,
    reserve: 32 << 20,
    floor: 8 << 20,
};
pub const RANGES_SHARE: crate::quota::Share =
    crate::quota::Share::quarter(RANGES_PER_VM as u64, 16);
pub const POOL_BYTES_SHARE: crate::quota::Share =
    crate::quota::Share::quarter(POOL_BYTES_PER_VM, 8 << 20);

/// UVM_ALLOC_SEMAPHORE_POOL and UVM_FREE (uvm_ioctl.h).
pub const ALLOC_SEMAPHORE_POOL: u32 = 68;
pub const FREE: u32 = 34;

const PAGE: u64 = 4096;

/// A pool mapped into the aperture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub aperture_off: u64,
    pub mapping_id: u32,
    /// MMAP replies not yet given back by a MUNMAP.
    pub refs: u32,
}

/// A pool a UVM file made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UvmRange {
    pub len: u64,
    pub placed: Option<Placement>,
}

/// What an MMAP of a pool should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmapPlan {
    /// Already placed: reply with it (the caller counts the reference with
    /// [`UvmMaps::add_ref`]).
    Existing(Placement),
    /// Place it here; the chunks are reserved until [`UvmMaps::commit`] or
    /// [`UvmMaps::abort`].
    New { aperture_off: u64 },
}

/// A placement to withdraw from the VMM: `(aperture offset, length)`.
pub type Withdraw = (u64, u64);

#[derive(Debug, Default)]
pub struct UvmMaps {
    /// By `(handle, base)`.
    ranges: BTreeMap<(u32, u64), UvmRange>,
    /// Handles whose VA space is in multi-process sharing mode.
    shared: HashSet<u32>,
    /// Bytes of aperture this session may use; 0 when there is none.
    aperture_len: u64,
    /// One per [`CHUNK`] of `aperture_len`.
    used: Vec<bool>,
    /// The guest process each UVM file is charged to (quota.rs).
    owners: HashMap<u32, Owner>,
}

impl UvmMaps {
    /// The aperture this session has, in bytes (a multiple of [`CHUNK`], at
    /// most [`APERTURE_MAX`]). Ignored while anything is placed.
    pub fn set_aperture(&mut self, len: u64) {
        if self.placements() > 0 {
            log::warn!("uvm aperture: not resized under live placements");
            return;
        }
        let len = len.min(APERTURE_MAX) & !(CHUNK - 1);
        self.aperture_len = len;
        self.used = vec![false; (len / CHUNK) as usize];
    }

    pub fn aperture_len(&self) -> u64 {
        self.aperture_len
    }

    /// UVM_INITIALIZE put `handle`'s VA space in sharing mode.
    pub fn mark_shared(&mut self, handle: u32) {
        self.shared.insert(handle);
    }

    pub fn is_shared(&self, handle: u32) -> bool {
        self.shared.contains(&handle)
    }

    /// `handle` is charged to `owner`.
    pub fn set_owner(&mut self, handle: u32, owner: Owner) {
        if owner == Owner::Unknown {
            self.owners.remove(&handle);
        } else {
            self.owners.insert(handle, owner);
        }
    }

    fn owner(&self, handle: u32) -> Owner {
        self.owners.get(&handle).copied().unwrap_or_default()
    }

    /// Whether UVM may be asked to make a pool of `len` bytes on `handle`:
    /// a length that could ever be mapped, and room in the file's, the
    /// VM's and the process's pool budgets. Checked before the host call,
    /// because UVM allocates the pool's memory in the host kernel there and
    /// then, whether or not it is ever mapped (F1). The errno on refusal.
    pub fn admit_pool(&self, handle: u32, len: u64) -> Result<(), i32> {
        if len == 0 || len > MAX_LEN {
            log::warn!(
                "uvm: a {len:#x}-byte pool on handle {handle} refused (at most {MAX_LEN:#x})"
            );
            return Err(libc::EINVAL);
        }
        let owner = self.owner(handle);
        let (mut file_n, mut file_b, mut own_n, mut own_b, mut vm_b) =
            (0usize, 0u64, 0u64, 0u64, 0u64);
        for (&(h, _), r) in &self.ranges {
            vm_b += r.len;
            if h == handle {
                file_n += 1;
                file_b += r.len;
            }
            if owner != Owner::Unknown && self.owner(h) == owner {
                own_n += 1;
                own_b += r.len;
            }
        }
        let vm_n = self.ranges.len();
        if file_n >= RANGES_PER_FILE
            || file_b + len > POOL_BYTES_PER_FILE
            || admits(
                &RANGES_SHARE,
                owner,
                own_n,
                1,
                vm_n as u64,
                RANGES_PER_VM as u64,
            )
            .is_err()
            || admits(
                &POOL_BYTES_SHARE,
                owner,
                own_b,
                len,
                vm_b,
                POOL_BYTES_PER_VM,
            )
            .is_err()
        {
            log::warn!(
                "uvm: a {len:#x}-byte pool on handle {handle} refused: the file has {file_n} \
                 ({file_b:#x} bytes), its process {owner:?} {own_n} ({own_b:#x}), the VM \
                 {vm_n} ({vm_b:#x})"
            );
            return Err(libc::ENOMEM);
        }
        Ok(())
    }

    /// ALLOC_SEMAPHORE_POOL succeeded on `handle`. False (and nothing kept)
    /// past a budget or for a pool too large ever to be mapped; a record
    /// already at that key (UVM refuses overlapping ranges, so it would be
    /// stale) is replaced, and its placement, if any, returned to withdraw.
    pub fn record(&mut self, handle: u32, base: u64, len: u64) -> (bool, Option<Withdraw>) {
        let stale = self.forget(handle, base);
        if len == 0 || len > MAX_LEN {
            log::debug!("uvm: pool {base:#x}+{len:#x} of handle {handle} is not mappable here");
            return (false, stale);
        }
        let of_file = self.ranges.range((handle, 0)..=(handle, u64::MAX)).count();
        if of_file >= RANGES_PER_FILE || self.ranges.len() >= RANGES_PER_VM {
            log::warn!(
                "uvm: pool {base:#x}+{len:#x} of handle {handle} not recorded: {of_file} of \
                 {RANGES_PER_FILE} for the file, {} of {RANGES_PER_VM} for the VM",
                self.ranges.len()
            );
            return (false, stale);
        }
        self.ranges
            .insert((handle, base), UvmRange { len, placed: None });
        (true, stale)
    }

    /// FREE of `base` succeeded on `handle`: the record goes, and with it a
    /// placement to withdraw -- which UVM should have made impossible, since
    /// it refuses to free a pool that is still mapped.
    pub fn forget(&mut self, handle: u32, base: u64) -> Option<Withdraw> {
        let r = self.ranges.remove(&(handle, base))?;
        let p = r.placed?;
        self.free_chunks(p.aperture_off, r.len);
        Some((p.aperture_off, r.len))
    }

    /// Check an MMAP of `handle` at `base` for `len` bytes with `prot`, and
    /// reserve aperture space for it. The errno on refusal.
    pub fn plan_mmap(
        &mut self,
        handle: u32,
        base: u64,
        len: u64,
        prot: u32,
    ) -> Result<MmapPlan, i32> {
        if self.aperture_len == 0 || !self.is_shared(handle) {
            return Err(libc::EINVAL);
        }
        if prot != 3 {
            return Err(libc::EINVAL);
        }
        // UVM itself takes nothing but the exact range (uvm.c:872).
        let Some(r) = self.ranges.get(&(handle, base)).copied() else {
            return Err(libc::EINVAL);
        };
        if r.len != len {
            return Err(libc::EINVAL);
        }
        if let Some(p) = r.placed {
            return Ok(MmapPlan::Existing(p));
        }
        let Some(end) = base.checked_add(len) else {
            return Err(libc::EINVAL);
        };
        if base < HVA_MIN
            || end > HVA_MAX
            || len > MAX_LEN
            || !base.is_multiple_of(PAGE)
            || !len.is_multiple_of(PAGE)
        {
            return Err(libc::EINVAL);
        }
        // One address space in the VMM for all of them. Refused as any
        // placement that cannot be made is: no errno of its own.
        let clash = self
            .ranges
            .iter()
            .any(|(&(_, b), o)| o.placed.is_some() && b < end && base < b.saturating_add(o.len));
        if clash {
            log::debug!(
                "uvm: mmap of {base:#x}+{len:#x} on handle {handle}: a live pool covers it"
            );
            return Err(libc::ENOMEM);
        }
        let owner = self.owner(handle);
        let (mut file_n, mut file_b, mut vm_n, mut vm_b) = (0usize, 0u64, 0usize, 0u64);
        let (mut own_n, mut own_b) = (0u64, 0u64);
        for (&(h, _), o) in &self.ranges {
            if o.placed.is_some() {
                vm_n += 1;
                vm_b += o.len;
                if h == handle {
                    file_n += 1;
                    file_b += o.len;
                }
                if owner != Owner::Unknown && self.owner(h) == owner {
                    own_n += 1;
                    own_b += o.len;
                }
            }
        }
        if file_n >= MAPS_PER_FILE
            || vm_n >= MAPS_PER_VM
            || file_b + len > BYTES_PER_FILE
            || vm_b + len > BYTES_PER_VM
            || admits(
                &MAPS_SHARE,
                owner,
                own_n,
                1,
                vm_n as u64,
                MAPS_PER_VM as u64,
            )
            .is_err()
            || admits(&BYTES_SHARE, owner, own_b, len, vm_b, BYTES_PER_VM).is_err()
        {
            log::warn!(
                "uvm: mmap of {base:#x}+{len:#x} on handle {handle} over budget: file {file_n} \
                 placement(s) {file_b:#x} bytes, VM {vm_n} placement(s) {vm_b:#x} bytes"
            );
            return Err(libc::ENOMEM);
        }
        let need = len.div_ceil(CHUNK) as usize;
        let Some(first) = self.first_fit(need) else {
            return Err(libc::ENOMEM);
        };
        for u in &mut self.used[first..first + need] {
            *u = true;
        }
        Ok(MmapPlan::New {
            aperture_off: first as u64 * CHUNK,
        })
    }

    /// The VMM placed `(handle, base)` at `aperture_off`, under `mapping_id`.
    pub fn commit(&mut self, handle: u32, base: u64, aperture_off: u64, mapping_id: u32) {
        if let Some(r) = self.ranges.get_mut(&(handle, base)) {
            r.placed = Some(Placement {
                aperture_off,
                mapping_id,
                refs: 1,
            });
        }
    }

    /// The VMM would not place it: give the reserved chunks back.
    pub fn abort(&mut self, aperture_off: u64, len: u64) {
        self.free_chunks(aperture_off, len);
    }

    /// Another MMAP reply of a placed pool.
    pub fn add_ref(&mut self, handle: u32, base: u64) {
        if let Some(p) = self
            .ranges
            .get_mut(&(handle, base))
            .and_then(|r| r.placed.as_mut())
        {
            p.refs = p.refs.saturating_add(1);
        }
    }

    /// MUNMAP of `mapping_id` on `handle`. `None`: not a UVM placement of
    /// that handle. `Some(None)`: a reference went, others remain.
    /// `Some(Some(w))`: the last went; withdraw `w` (its chunks are free).
    pub fn munmap(&mut self, handle: u32, mapping_id: u32) -> Option<Option<Withdraw>> {
        let (off, len, last) = {
            let r = self
                .ranges
                .range_mut((handle, 0)..=(handle, u64::MAX))
                .map(|(_, r)| r)
                .find(|r| r.placed.is_some_and(|p| p.mapping_id == mapping_id))?;
            let p = r.placed.as_mut().expect("found by it");
            p.refs = p.refs.saturating_sub(1);
            let last = p.refs == 0;
            let off = p.aperture_off;
            if last {
                r.placed = None;
            }
            (off, r.len, last)
        };
        if !last {
            return Some(None);
        }
        self.free_chunks(off, len);
        Some(Some((off, len)))
    }

    /// `handle` is closing: every placement of it to withdraw, and its
    /// records and sharing mark gone.
    pub fn take_handle(&mut self, handle: u32) -> Vec<Withdraw> {
        self.shared.remove(&handle);
        self.owners.remove(&handle);
        let keys: Vec<_> = self
            .ranges
            .range((handle, 0)..=(handle, u64::MAX))
            .map(|(k, _)| *k)
            .collect();
        keys.into_iter()
            .filter_map(|(h, b)| self.forget(h, b))
            .collect()
    }

    /// The session ends: every placement to withdraw, and no aperture until
    /// the next HELLO says there is one.
    pub fn take_all(&mut self) -> Vec<Withdraw> {
        let out = self
            .ranges
            .values()
            .filter_map(|r| r.placed.map(|p| (p.aperture_off, r.len)))
            .collect();
        *self = Self::default();
        out
    }

    /// Whether a live placement has `id`.
    pub fn has_mapping_id(&self, id: u32) -> bool {
        self.ranges
            .values()
            .any(|r| r.placed.is_some_and(|p| p.mapping_id == id))
    }

    /// Live placements.
    pub fn placements(&self) -> usize {
        self.ranges.values().filter(|r| r.placed.is_some()).count()
    }

    fn first_fit(&self, need: usize) -> Option<usize> {
        let mut run = 0;
        for (i, &u) in self.used.iter().enumerate() {
            run = if u { 0 } else { run + 1 };
            if run == need {
                return Some(i + 1 - need);
            }
        }
        None
    }

    fn free_chunks(&mut self, off: u64, len: u64) {
        let first = (off / CHUNK) as usize;
        let n = len.div_ceil(CHUNK) as usize;
        for u in self.used.iter_mut().skip(first).take(n) {
            *u = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const X: u64 = 0x2_06e0_0000;
    const MB2: u64 = 2 << 20;

    fn maps(aperture: u64) -> UvmMaps {
        let mut m = UvmMaps::default();
        m.set_aperture(aperture);
        m.mark_shared(1);
        m.mark_shared(2);
        m
    }

    fn place(m: &mut UvmMaps, h: u32, base: u64, len: u64, id: u32) -> u64 {
        match m.plan_mmap(h, base, len, 3) {
            Ok(MmapPlan::New { aperture_off }) => {
                m.commit(h, base, aperture_off, id);
                aperture_off
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn only_the_exact_recorded_pool_read_write_is_mapped() {
        let mut m = maps(APERTURE_MAX);
        assert_eq!(m.record(1, X, MB2), (true, None));
        assert_eq!(m.plan_mmap(1, X + 4096, MB2 - 4096, 3), Err(libc::EINVAL));
        assert_eq!(m.plan_mmap(1, X, MB2 - 4096, 3), Err(libc::EINVAL));
        assert_eq!(m.plan_mmap(1, X, 2 * MB2, 3), Err(libc::EINVAL));
        assert_eq!(m.plan_mmap(1, X, MB2, 1), Err(libc::EINVAL), "read-only");
        assert_eq!(
            m.plan_mmap(2, X, MB2, 3),
            Err(libc::EINVAL),
            "another file's"
        );
        assert_eq!(
            m.plan_mmap(1, X, MB2, 3),
            Ok(MmapPlan::New { aperture_off: 0 })
        );
    }

    #[test]
    fn nothing_is_mapped_without_an_aperture_or_sharing_mode() {
        let mut m = UvmMaps::default();
        m.mark_shared(1);
        m.record(1, X, MB2);
        assert_eq!(m.plan_mmap(1, X, MB2, 3), Err(libc::EINVAL));
        let mut m = UvmMaps::default();
        m.set_aperture(APERTURE_MAX);
        m.record(1, X, MB2);
        assert_eq!(m.plan_mmap(1, X, MB2, 3), Err(libc::EINVAL));
    }

    #[test]
    fn the_aperture_is_capped_and_whole_chunks() {
        let mut m = UvmMaps::default();
        m.set_aperture(64 << 30);
        assert_eq!(m.aperture_len(), APERTURE_MAX);
        m.set_aperture(5 << 20);
        assert_eq!(m.aperture_len(), 4 << 20);
    }

    #[test]
    fn the_host_address_band_is_enforced_at_both_edges() {
        assert_eq!((HVA_MIN, HVA_MAX), (4 << 30, 32 << 40));
        let mut m = maps(APERTURE_MAX);
        for (base, ok) in [
            (HVA_MIN - 4096, false),
            (HVA_MIN, true),
            (HVA_MAX - MB2, true),
            (HVA_MAX - MB2 + 4096, false),
            // A legacy (bottom-up) mmap layout starts at a third of the
            // 47-bit space, where the VMM's own mappings would be.
            (((1u64 << 47) / 3) & !(MB2 - 1), false),
            (!4095, false),
        ] {
            m.record(1, base, MB2);
            let r = m.plan_mmap(1, base, MB2, 3);
            assert_eq!(r.is_ok(), ok, "{base:#x}: {r:?}");
            if let Ok(MmapPlan::New { aperture_off }) = r {
                m.abort(aperture_off, MB2);
            }
        }
    }

    #[test]
    fn a_pool_too_large_is_never_recorded() {
        let mut m = maps(APERTURE_MAX);
        assert_eq!(m.record(1, X, MAX_LEN + 4096), (false, None));
        assert_eq!(m.record(1, X, 0), (false, None));
        assert_eq!(m.plan_mmap(1, X, MAX_LEN + 4096, 3), Err(libc::EINVAL));
    }

    #[test]
    #[cfg_attr(miri, ignore = "fills a cap of thousands: too slow under Miri")]
    fn recorded_ranges_are_bounded_per_file_and_per_vm() {
        let mut m = maps(APERTURE_MAX);
        for i in 0..RANGES_PER_FILE as u64 {
            assert!(m.record(1, X + i * MB2, MB2).0);
        }
        assert!(!m.record(1, X + 1000 * MB2, MB2).0);
        assert!(m.record(2, X, MB2).0, "another file has its own");
        let mut m = UvmMaps::default();
        for i in 0..RANGES_PER_VM as u64 {
            assert!(m.record((i / 200) as u32, X + i * MB2, MB2).0);
        }
        assert!(!m.record(999, X, MB2).0);
    }

    #[test]
    fn placements_are_bounded_per_file_and_per_vm_by_count_and_bytes() {
        // Count per file.
        let mut m = maps(APERTURE_MAX);
        for i in 0..=MAPS_PER_FILE as u64 {
            m.record(1, X + i * MB2, 4096);
        }
        for i in 0..MAPS_PER_FILE as u64 {
            place(&mut m, 1, X + i * MB2, 4096, i as u32 + 1);
        }
        assert_eq!(
            m.plan_mmap(1, X + MAPS_PER_FILE as u64 * MB2, 4096, 3),
            Err(libc::ENOMEM)
        );
        // Bytes per file.
        let mut m = maps(APERTURE_MAX);
        m.record(1, X, MAX_LEN);
        m.record(1, X + MAX_LEN, 4096);
        place(&mut m, 1, X, MAX_LEN, 1);
        assert_eq!(m.plan_mmap(1, X + MAX_LEN, 4096, 3), Err(libc::ENOMEM));
        // Count and bytes per VM.
        let mut m = maps(APERTURE_MAX);
        let files = (MAPS_PER_VM / MAPS_PER_FILE) as u32;
        for f in 0..=files {
            m.mark_shared(10 + f);
            for i in 0..MAPS_PER_FILE as u64 {
                m.record(10 + f, X + (f as u64 * 64 + i) * MB2, 4096);
            }
        }
        let mut id = 1;
        for f in 0..files {
            for i in 0..MAPS_PER_FILE as u64 {
                place(&mut m, 10 + f, X + (f as u64 * 64 + i) * MB2, 4096, id);
                id += 1;
            }
        }
        assert_eq!(
            m.plan_mmap(10 + files, X + files as u64 * 64 * MB2, 4096, 3),
            Err(libc::ENOMEM)
        );
        let mut m = maps(APERTURE_MAX);
        let files = (BYTES_PER_VM / MAX_LEN) as u32;
        for f in 0..=files {
            m.mark_shared(10 + f);
            m.record(10 + f, X + f as u64 * MAX_LEN, MAX_LEN);
        }
        for f in 0..files {
            place(&mut m, 10 + f, X + f as u64 * MAX_LEN, MAX_LEN, f + 1);
        }
        assert_eq!(
            m.plan_mmap(10 + files, X + files as u64 * MAX_LEN, MAX_LEN, 3),
            Err(libc::ENOMEM)
        );
    }

    #[test]
    fn the_aperture_is_first_fit_and_reused_after_abort_and_munmap() {
        let mut m = maps(8 * MB2);
        for i in 0..4 {
            m.record(1, X + i * 16 * MB2, 2 * MB2);
        }
        m.record(2, X + 100 * MB2, 4096);
        assert_eq!(place(&mut m, 1, X, 2 * MB2, 1), 0);
        assert_eq!(place(&mut m, 1, X + 16 * MB2, 2 * MB2, 2), 2 * MB2);
        assert_eq!(place(&mut m, 1, X + 32 * MB2, 2 * MB2, 3), 4 * MB2);
        assert_eq!(m.munmap(1, 1), Some(Some((0, 2 * MB2))));
        // A 4 KiB pool takes a whole chunk, the first free one...
        assert_eq!(place(&mut m, 2, X + 100 * MB2, 4096, 9), 0);
        // ...which leaves a one-chunk hole a 4 MiB pool does not fit in.
        assert_eq!(place(&mut m, 1, X + 48 * MB2, 2 * MB2, 4), 6 * MB2);
        assert_eq!(m.plan_mmap(1, X, 2 * MB2, 3), Err(libc::ENOMEM));
        // Freed chunks join the hole.
        m.munmap(1, 2);
        let plan = m.plan_mmap(1, X, 2 * MB2, 3);
        assert_eq!(plan, Ok(MmapPlan::New { aperture_off: MB2 }));
        m.abort(MB2, 2 * MB2);
        assert_eq!(
            m.plan_mmap(1, X, 2 * MB2, 3),
            Ok(MmapPlan::New { aperture_off: MB2 }),
            "and are free again after an abort"
        );
    }

    #[test]
    fn two_files_cannot_both_place_at_one_host_address() {
        let mut m = maps(APERTURE_MAX);
        m.record(1, X, MB2);
        m.record(2, X + 4096, MB2);
        place(&mut m, 1, X, MB2, 1);
        assert_eq!(m.plan_mmap(2, X + 4096, MB2, 3), Err(libc::ENOMEM));
        m.munmap(1, 1);
        assert!(m.plan_mmap(2, X + 4096, MB2, 3).is_ok());
    }

    #[test]
    fn a_second_mmap_shares_the_placement_and_the_last_munmap_withdraws_it() {
        let mut m = maps(APERTURE_MAX);
        m.record(1, X, MB2);
        let off = place(&mut m, 1, X, MB2, 7);
        let Ok(MmapPlan::Existing(p)) = m.plan_mmap(1, X, MB2, 3) else {
            panic!()
        };
        assert_eq!((p.aperture_off, p.mapping_id), (off, 7));
        m.add_ref(1, X);
        assert!(m.has_mapping_id(7));
        assert_eq!(
            m.munmap(2, 7),
            None,
            "another handle's MUNMAP matches nothing"
        );
        assert_eq!(m.munmap(1, 8), None);
        assert_eq!(m.munmap(1, 7), Some(None));
        assert_eq!(m.munmap(1, 7), Some(Some((off, MB2))));
        assert!(!m.has_mapping_id(7));
        assert_eq!(m.munmap(1, 7), None);
        // The record stays until FREE; mapping again is a new placement.
        assert!(matches!(
            m.plan_mmap(1, X, MB2, 3),
            Ok(MmapPlan::New { .. })
        ));
    }

    #[test]
    fn free_forgets_the_record_and_hands_back_a_placement_it_should_not_have() {
        let mut m = maps(APERTURE_MAX);
        m.record(1, X, MB2);
        assert_eq!(m.forget(1, X), None);
        assert_eq!(m.plan_mmap(1, X, MB2, 3), Err(libc::EINVAL));
        m.record(1, X, MB2);
        let off = place(&mut m, 1, X, MB2, 1);
        assert_eq!(m.forget(1, X), Some((off, MB2)));
        assert_eq!(m.placements(), 0);
        // A stale record replaced by a new ALLOC hands its placement back.
        m.record(1, X, MB2);
        let off = place(&mut m, 1, X, MB2, 2);
        assert_eq!(m.record(1, X, MB2), (true, Some((off, MB2))));
    }

    #[test]
    fn closing_a_file_takes_its_placements_and_a_reset_takes_everything() {
        let mut m = maps(APERTURE_MAX);
        m.record(1, X, MB2);
        m.record(1, X + 16 * MB2, MB2);
        m.record(2, X + 32 * MB2, MB2);
        let a = place(&mut m, 1, X, MB2, 1);
        place(&mut m, 2, X + 32 * MB2, MB2, 2);
        assert_eq!(m.take_handle(1), vec![(a, MB2)]);
        assert!(!m.is_shared(1));
        assert_eq!(m.placements(), 1);
        assert_eq!(m.take_all().len(), 1);
        assert_eq!(m.aperture_len(), 0);
        assert!(!m.is_shared(2));
        assert_eq!(m.placements(), 0);
    }

    /// A pool is host kernel memory from the moment UVM makes it: its length
    /// and the pool budgets are checked before UVM is asked (F1).
    #[test]
    fn a_pool_that_could_never_be_mapped_is_refused_before_uvm_makes_it() {
        let mut m = maps(APERTURE_MAX);
        assert_eq!(m.admit_pool(1, 0), Err(libc::EINVAL));
        assert_eq!(m.admit_pool(1, MAX_LEN + 4096), Err(libc::EINVAL));
        assert_eq!(m.admit_pool(1, MAX_LEN), Ok(()));
        // The file's bytes of pools, mapped or not.
        for i in 0..4 {
            assert_eq!(m.admit_pool(1, MAX_LEN), Ok(()));
            assert!(m.record(1, X + i * MAX_LEN, MAX_LEN).0);
        }
        assert_eq!(m.admit_pool(1, 4096), Err(libc::ENOMEM));
        assert_eq!(m.admit_pool(2, 4096), Ok(()), "another file has its own");
    }

    /// One process's files together hold at most its share of the
    /// placements, and the VM's last ones are kept for newcomers (B5).
    #[test]
    fn one_guest_process_holds_at_most_its_share_of_placements() {
        let mut m = UvmMaps::default();
        m.set_aperture(APERTURE_MAX);
        let p = |t: u32| Owner::Proc {
            tgid: t,
            start_ns: 1,
        };
        for h in 1..=4 {
            m.mark_shared(h);
            m.set_owner(h, p(1));
        }
        m.mark_shared(9);
        m.set_owner(9, p(2));
        let mut placed = 0;
        'files: for h in 1..=4u32 {
            for i in 0..MAPS_PER_FILE as u64 {
                let base = X + (u64::from(h) * 64 + i) * MB2;
                m.record(h, base, MB2);
                match m.plan_mmap(h, base, MB2, 3) {
                    Ok(MmapPlan::New { aperture_off }) => {
                        m.commit(h, base, aperture_off, placed + 1);
                        placed += 1;
                    }
                    Err(e) => {
                        assert_eq!(e, libc::ENOMEM);
                        continue 'files;
                    }
                    other => panic!("{other:?}"),
                }
            }
        }
        assert_eq!(
            placed as usize, MAPS_PER_FILE,
            "one process's share, over four files"
        );
        m.record(9, X, MB2);
        assert!(matches!(
            m.plan_mmap(9, X, MB2, 3),
            Ok(MmapPlan::New { .. })
        ));
    }
}
