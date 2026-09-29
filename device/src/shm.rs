// SPDX-License-Identifier: Apache-2.0
// device/src/shm.rs

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::os::fd::RawFd;
use std::sync::Arc;

use crate::error::{DeviceError, Result};
use crate::quota::{Ledger, Owner, Share};
use crate::sys::mem::Window;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgprotKind {
    WriteBack = 0,
    WriteCombine = 1,
    Uncached = 2,
}

impl PgprotKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::WriteBack),
            1 => Some(Self::WriteCombine),
            2 => Some(Self::Uncached),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ShmRegion {
    pub offset: u64,
    pub length: u64,
    pub pgprot: PgprotKind,
}

/// One page-protection zone, with a free list.
///
/// This was a bump allocator: `alloc` advanced a cursor and nothing ever gave
/// space back. Captured traces make the consequence concrete -- a single
/// 3-second 1080p `h264_nvenc` encode maps ~116 MiB into the write-combine
/// zone across 68 mappings, and unmaps 66 of them at teardown. Without a free
/// path the cursor keeps that 116 MiB forever, so the *second* encode in the
/// same guest fails with ENOMEM on a 128 MiB zone.
struct Zone {
    base: u64,
    size: u64,
    /// Free extents as `offset -> length`, offsets relative to `base`, kept
    /// disjoint and coalesced.
    free: BTreeMap<u64, u64>,
    /// Bytes each guest process holds of the zone, and how many it may
    /// (quota.rs): by default half the zone, and the last eighth only
    /// while it holds at most a sixteenth (`--window-owner-share`,
    /// `Share::percent`). One process mapping a whole zone left every other
    /// process of the VM with ENOMEM (B2).
    held: Ledger,
    share: Share,
    /// What the zone has seen, for the teardown summary: operators size the
    /// window by it.
    peak: Peak,
}

/// The most of one zone ever in use, by everyone and by one process, and
/// what was refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Peak {
    /// Bytes in use at once.
    pub used: u64,
    /// Bytes one guest process held at once.
    pub one_process: u64,
    /// The largest single extent.
    pub largest: u64,
    /// Extents handed out.
    pub allocs: u64,
    /// Requests refused because the process held its share, or only the
    /// reserve was left (quota.rs), and because no free extent was large
    /// enough (the zone full, or fragmented).
    pub refused_share: u64,
    pub refused_space: u64,
}

impl Zone {
    fn new(base: u64, size: u64, share: Share) -> Self {
        let mut free = BTreeMap::new();
        if size > 0 {
            free.insert(0, size);
        }
        Self {
            base,
            size,
            free,
            held: Ledger::default(),
            share,
            peak: Peak::default(),
        }
    }

    /// First-fit. Returns an absolute offset, or `None` if no extent fits.
    fn alloc(&mut self, length: u64) -> Option<u64> {
        let want = align_up(length, PAGE_SIZE)?;
        if want == 0 {
            return None;
        }
        let (&start, &len) = self.free.iter().find(|&(_, &len)| len >= want)?;
        self.free.remove(&start);
        if len > want {
            self.free.insert(start + want, len - want);
        }
        Some(self.base + start)
    }

    /// Return an extent to the zone, coalescing with either neighbour.
    ///
    /// Returns false if the extent is not inside this zone or overlaps a range
    /// already free, which would mean a double free.
    fn free_extent(&mut self, offset: u64, length: u64) -> bool {
        let Some(want) = align_up(length, PAGE_SIZE) else {
            return false;
        };
        if want == 0 || offset < self.base {
            return false;
        }
        let start = offset - self.base;
        if start.checked_add(want).is_none_or(|end| end > self.size) {
            return false;
        }

        // Overlap with an existing free extent means this was freed already.
        if let Some((&ps, &pl)) = self.free.range(..=start).next_back()
            && ps + pl > start
        {
            return false;
        }
        if let Some((&ns, _)) = self.free.range(start..).next()
            && start + want > ns
        {
            return false;
        }

        let mut s = start;
        let mut l = want;

        // Coalesce with the extent below, if it ends exactly here.
        if let Some((&ps, &pl)) = self.free.range(..s).next_back()
            && ps + pl == s
        {
            self.free.remove(&ps);
            s = ps;
            l += pl;
        }
        // Coalesce with the extent above, if it starts exactly at our end.
        if let Some((&ns, &nl)) = self.free.range(s + l..).next()
            && s + l == ns
        {
            self.free.remove(&ns);
            l += nl;
        }

        self.free.insert(s, l);
        true
    }

    fn free_bytes(&self) -> u64 {
        self.free.values().sum()
    }

    /// The largest single allocation this zone could still satisfy.
    fn largest_free(&self) -> u64 {
        self.free.values().copied().max().unwrap_or(0)
    }
}

/// The shared window: its three zones' sizes, and how much of each one
/// guest process may hold. One value sizes the allocator and answers the
/// VMM's GET_SHMEM_CONFIG, so the two cannot disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZoneConfig {
    pub uc_size: u64,
    pub wc_size: u64,
    pub wb_size: u64,
    /// Percent of each zone one guest process may hold
    /// ([`Share::percent`]); 50 is [`Share::half`].
    pub owner_percent: u8,
}

const MIB: u64 = 1 << 20;

impl ZoneConfig {
    /// `--window-size`'s default, in MiB: [`ZoneConfig::default_1gib`].
    pub const DEFAULT_MIB: u64 = 1024;
    /// The smallest window, in MiB: a quarter of the default, whose
    /// write-combining zone (192 MiB) still holds the largest single
    /// mapping measured (56 MiB) with a share to spare.
    pub const MIN_MIB: u64 = 256;
    /// A window's size is a multiple of this, in MiB, so that every zone of
    /// [`ZoneConfig::for_window`] is a whole number of 2 MiB pages.
    pub const ALIGN_MIB: u64 = 64;
    /// The largest one shared memory region crosvm takes
    /// (`MAX_SHARED_MEMORY_REGION_SIZE`, patches/crosvm/0006), in MiB. The
    /// window and the UVM aperture share one BAR there, so the two together
    /// are held to it.
    pub const MAX_REGION_MIB: u64 = 64 * 1024;
    /// `--window-owner-share`'s default, and its bounds, in percent.
    pub const DEFAULT_OWNER_PERCENT: u8 = 50;
    pub const MIN_OWNER_PERCENT: u8 = 1;
    pub const MAX_OWNER_PERCENT: u8 = 95;

    /// The window `--window-size` and `--window-owner-share` ask for, or
    /// why it cannot be had. `aperture` is the UVM aperture beside it in
    /// bytes (0 without compute).
    ///
    /// How the zones grow past the default's 1 GiB (UC 32, WC 768, WB 224
    /// MiB): the uncached zone stays at 32 MiB -- it holds registers, a
    /// 64 KiB doorbell per channel user, whose number follows processes and
    /// not how much memory they map (no workload measured has used half a
    /// MiB of it) -- and the rest is split between write-combining and
    /// write-back 24:7, as in the default, so WC takes 77 % of the growth.
    /// WB keeps its proportion because the rig's heaviest applications use
    /// it most (Blender held 86 MiB of WB to 35 MiB of WC), and a write-back
    /// mapping that does not fit is placed write-combining
    /// (`NvidiaBackend::alloc_zone`) -- correct, but slow to read -- and
    /// never the other way: WC has to hold its own and WB's overflow. Below
    /// 1 GiB all three shrink in proportion. At 1 GiB this is
    /// `default_1gib` exactly. The measurements are in DEPLOY.md, "Sizing
    /// the window".
    pub fn for_window(
        mib: u64,
        owner_percent: u8,
        aperture: u64,
    ) -> std::result::Result<Self, String> {
        if !(Self::MIN_OWNER_PERCENT..=Self::MAX_OWNER_PERCENT).contains(&owner_percent) {
            return Err(format!(
                "--window-owner-share {owner_percent}: a guest process's share of each zone is \
                 {}-{} percent",
                Self::MIN_OWNER_PERCENT,
                Self::MAX_OWNER_PERCENT
            ));
        }
        if mib < Self::MIN_MIB || !mib.is_multiple_of(Self::ALIGN_MIB) {
            return Err(format!(
                "--window-size {mib}: the window is at least {} MiB and a multiple of {} MiB",
                Self::MIN_MIB,
                Self::ALIGN_MIB
            ));
        }
        let region = mib.saturating_mul(MIB).saturating_add(aperture);
        if region > Self::MAX_REGION_MIB * MIB {
            return Err(format!(
                "--window-size {mib}: the window{} would be {} MiB, and crosvm takes at most {} MiB \
                 of shared memory (MAX_SHARED_MEMORY_REGION_SIZE)",
                if aperture > 0 {
                    format!(" and the {} MiB UVM aperture", aperture / MIB)
                } else {
                    String::new()
                },
                region / MIB,
                Self::MAX_REGION_MIB
            ));
        }
        // In MiB: UC a thirty-second up to its 32 MiB, and the rest split
        // 24:7 between WC and WB as in the default, WB rounded down to 2 MiB.
        let d = Self::default_1gib();
        let (d_uc, d_wc, d_wb) = (d.uc_size / MIB, d.wc_size / MIB, d.wb_size / MIB);
        let uc = (mib / 32).min(d_uc);
        let wb = ((mib - uc) * d_wb / (d_wc + d_wb)) & !1;
        let (uc, wb) = (uc * MIB, wb * MIB);
        let cfg = Self {
            uc_size: uc,
            wc_size: mib * MIB - uc - wb,
            wb_size: wb,
            owner_percent,
        };
        debug_assert!(cfg.zones_aligned());
        Ok(cfg)
    }

    /// Whether every zone is a whole number of 2 MiB pages, as the
    /// allocator places a mapping at any page of its zone and the VMM maps
    /// the window with huge pages where it can.
    fn zones_aligned(&self) -> bool {
        [self.uc_size, self.wc_size, self.wb_size]
            .iter()
            .all(|z| z.is_multiple_of(2 * MIB))
    }

    /// The share of a zone of `size` bytes one guest process may hold.
    pub fn share(&self, size: u64) -> Share {
        Share::percent(size, self.owner_percent)
    }

    /// Zone sizes chosen from measured driver behaviour, not guessed.
    ///
    /// Captured traces on a Tesla T4 (580.178.04) show every mapping these
    /// workloads make landing in the **write-combine** zone; uncached and
    /// write-back were never touched at all. Peak concurrent write-combine
    /// use, with unmaps honoured:
    ///
    /// | workload | peak WC | largest single mapping |
    /// | --- | --- | --- |
    /// | `vulkaninfo` | 15.9 MiB | 4 MiB |
    /// | CUDA kernel launch | 67.6 MiB | 56 MiB |
    /// | `h264_nvenc` encode | 116.4 MiB | 56 MiB |
    /// | all three at once | 184.6 MiB | 56 MiB |
    ///
    /// The previous split gave write-combine 128 MiB, which one encode fills
    /// to 91% and three concurrent workloads overrun outright. This gives it
    /// roughly 4x the observed concurrent peak, and keeps the other two zones
    /// small but present -- they are unused by these workloads, which is not
    /// the same as unused in general.
    ///
    /// The window is a memfd, so pages are only committed when touched; the
    /// size is address space, not resident memory.
    ///
    /// **These numbers come from three workloads on one GPU, and are a floor
    /// rather than a bound.** `vulkaninfo` enumerates; it does not render. A
    /// game drawing at 4K will map more than any workload measured here, and
    /// the largest single mapping may grow past the 56 MiB seen so far -- which
    /// matters more than the totals, because a zone with enough free bytes can
    /// still refuse one large request if it has fragmented. Re-measure against
    /// a real render trace before treating this split as settled.
    ///
    /// **The traces also predate per-mapping classification.** Everything
    /// landed in write-combining then because the backend read the caching
    /// type RM returns, which the escape resets to DEFAULT for all but video
    /// memory (escape.c:600-601). Now system memory goes to write-back (it is
    /// allocated coherent, rmmem.rs) and doorbells to uncached, so part of
    /// the measured write-combine peak belongs to those zones. A write-back
    /// request that does not fit falls back to write-combining
    /// (`NvidiaBackend::alloc_zone`); an uncached one does not.
    pub fn default_1gib() -> Self {
        Self {
            uc_size: 32 * 1024 * 1024,
            wc_size: 768 * 1024 * 1024,
            wb_size: 224 * 1024 * 1024,
            owner_percent: Self::DEFAULT_OWNER_PERCENT,
        }
    }

    /// The original 256 MiB split. Too small for a single encode with any
    /// margin; kept only for tests that want a zone they can exhaust.
    pub fn default_256mib() -> Self {
        Self {
            uc_size: 4 * 1024 * 1024,
            wc_size: 128 * 1024 * 1024,
            wb_size: 124 * 1024 * 1024,
            owner_percent: Self::DEFAULT_OWNER_PERCENT,
        }
    }

    pub fn total(&self) -> u64 {
        self.uc_size + self.wc_size + self.wb_size
    }
}

pub struct ShmAllocator {
    uc: Zone,
    wc: Zone,
    wb: Zone,

    /// The window's own backing, a memfd mapped whole (sys::mem::Window),
    /// which an extent goes back to when it is freed. The guest's view of
    /// the window is the VMM's mapping; this one is the backend's.
    window: Arc<Window>,

    total_size: u64,
    /// `ZoneConfig::owner_percent`, for the summary.
    owner_percent: u8,

    /// Who each live extent is charged to, and its charged length, by its
    /// offset.
    owners: std::collections::HashMap<u64, (Owner, u64)>,
}

impl ShmAllocator {
    pub fn new(cfg: ZoneConfig) -> Self {
        assert_eq!(cfg.uc_size % 4096, 0);
        assert_eq!(cfg.wc_size % 4096, 0);
        assert_eq!(cfg.wb_size % 4096, 0);

        let total = cfg.total();
        assert!(total > 0);

        let window = Arc::new(
            Window::new(total as usize)
                .unwrap_or_else(|e| panic!("the shared window's backing ({total:#x} bytes): {e}")),
        );

        let uc_base = 0;
        let wc_base = cfg.uc_size;
        let wb_base = cfg.uc_size + cfg.wc_size;

        Self {
            uc: Zone::new(uc_base, cfg.uc_size, cfg.share(cfg.uc_size)),
            wc: Zone::new(wc_base, cfg.wc_size, cfg.share(cfg.wc_size)),
            wb: Zone::new(wb_base, cfg.wb_size, cfg.share(cfg.wb_size)),
            window,
            total_size: total,
            owner_percent: cfg.owner_percent,
            owners: std::collections::HashMap::new(),
        }
    }

    pub fn with_default_zones() -> Self {
        Self::new(ZoneConfig::default_1gib())
    }

    pub fn alloc(&mut self, length: u64, pgprot: PgprotKind) -> Result<ShmRegion> {
        self.alloc_for(length, pgprot, Owner::Unknown)
    }

    /// An extent of `length` in the `pgprot` zone, charged to `owner`,
    /// which may hold only its share of the zone (quota.rs).
    pub fn alloc_for(
        &mut self,
        length: u64,
        pgprot: PgprotKind,
        owner: Owner,
    ) -> Result<ShmRegion> {
        let zone = match pgprot {
            PgprotKind::Uncached => &mut self.uc,
            PgprotKind::WriteCombine => &mut self.wc,
            PgprotKind::WriteBack => &mut self.wb,
        };

        // A length the guest chose (an NVOS33's, an MMAP's) can be anything
        // up to u64::MAX: one larger than the zone is refused here, before
        // any arithmetic on it can overflow.
        let Some(want) = align_up(length, PAGE_SIZE).filter(|&w| w <= zone.size) else {
            zone.peak.refused_space += 1;
            return Err(DeviceError::Io(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!(
                    "SHM {pgprot:?} zone of {:#x} bytes cannot hold {length:#x}",
                    zone.size
                ),
            )));
        };
        let in_use = zone.size - zone.free_bytes();
        if let Err(why) = zone
            .held
            .admits(&zone.share, owner, want, in_use, zone.size)
            && why != crate::quota::Over::Pool
        {
            zone.peak.refused_share += 1;
            return Err(DeviceError::Io(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!(
                    "SHM {pgprot:?} zone: guest process {owner:?} holds {:#x} of {:#x} \
                     bytes and may not take {want:#x} more ({why:?})",
                    zone.held.held(owner),
                    zone.size
                ),
            )));
        }
        match zone.alloc(length) {
            Some(offset) => {
                zone.held.charge(owner, want);
                self.owners.insert(offset, (owner, want));
                let p = &mut zone.peak;
                p.used = p.used.max(in_use + want);
                p.one_process = p.one_process.max(zone.held.held(owner));
                p.largest = p.largest.max(want);
                p.allocs += 1;
                Ok(ShmRegion {
                    offset,
                    length,
                    pgprot,
                })
            }
            None => {
                zone.peak.refused_space += 1;
                Err(DeviceError::Io(std::io::Error::new(
                    std::io::ErrorKind::OutOfMemory,
                    format!(
                        "SHM {:?} zone cannot satisfy {} bytes: {} free in total but \
                         largest contiguous extent is {} ({} free extents)",
                        pgprot,
                        length,
                        zone.free_bytes(),
                        zone.largest_free(),
                        zone.free.len()
                    ),
                )))
            }
        }
    }

    /// What each zone has seen since the allocator was made, as
    /// `(uc, wc, wb)`.
    pub fn peaks(&self) -> (Peak, Peak, Peak) {
        (self.uc.peak, self.wc.peak, self.wb.peak)
    }

    /// One line an operator sizes the window by: each zone's size, the
    /// most of it in use at once, the most one guest process held, the
    /// largest mapping, and the refusals. Logged at teardown.
    pub fn usage_summary(&self) -> String {
        let mib = |b: u64| b as f64 / MIB as f64;
        let zone = |name: &str, z: &Zone| {
            let p = z.peak;
            format!(
                "{name} {:.0} MiB: peak {:.1} MiB in use, {:.1} MiB by one process, largest \
                 {:.1} MiB, {} mappings, refused {} for the share and {} for space",
                mib(z.size),
                mib(p.used),
                mib(p.one_process),
                mib(p.largest),
                p.allocs,
                p.refused_share,
                p.refused_space
            )
        };
        format!(
            "window {:.0} MiB, {}% per process: {}; {}; {}",
            mib(self.total_size),
            self.owner_percent,
            zone("UC", &self.uc),
            zone("WC", &self.wc),
            zone("WB", &self.wb)
        )
    }

    /// mmap a host fd into the SHM region at the given offset.
    /// Return a region to its zone and restore its SHM backing.
    ///
    /// Restoring the backing and reclaiming the extent must happen together.
    /// Doing only the first leaks the address range -- which is what the bump
    /// allocator did, and why a second NVENC encode in one guest ran the
    /// write-combine zone out of space.
    pub fn free(&mut self, region: &ShmRegion) -> Result<()> {
        let len = align_up(region.length, PAGE_SIZE).ok_or_else(|| {
            DeviceError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("SHM free of a region of {:#x} bytes", region.length),
            ))
        })?;

        // Put the memfd back under this range before the extent can be handed
        // to another mapping; the guest keeps the whole window mapped, so the
        // range must never be left without backing.
        self.unmap_host_fd(region.offset, len)?;

        let zone = match region.pgprot {
            PgprotKind::Uncached => &mut self.uc,
            PgprotKind::WriteCombine => &mut self.wc,
            PgprotKind::WriteBack => &mut self.wb,
        };
        if !zone.free_extent(region.offset, len) {
            return Err(DeviceError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "SHM free of {:?} region at {:#x}+{:#x} is out of range or already free",
                    region.pgprot, region.offset, len
                ),
            )));
        }
        if let Some((owner, charged)) = self.owners.remove(&region.offset) {
            zone.held.refund(owner, charged);
        }
        Ok(())
    }

    /// Bytes `owner` holds of the `pgprot` zone.
    pub fn held_by(&self, pgprot: PgprotKind, owner: Owner) -> u64 {
        match pgprot {
            PgprotKind::Uncached => &self.uc,
            PgprotKind::WriteCombine => &self.wc,
            PgprotKind::WriteBack => &self.wb,
        }
        .held
        .held(owner)
    }

    /// Free bytes remaining in each zone, as `(uc, wc, wb)`.
    pub fn free_bytes(&self) -> (u64, u64, u64) {
        (
            self.uc.free_bytes(),
            self.wc.free_bytes(),
            self.wb.free_bytes(),
        )
    }

    /// Largest single allocation each zone could still satisfy.
    pub fn largest_free(&self) -> (u64, u64, u64) {
        (
            self.uc.largest_free(),
            self.wc.largest_free(),
            self.wb.largest_free(),
        )
    }

    /// Place `length` bytes of `host_fd` at `shm_offset` of the window's
    /// local backing.
    pub fn map_host_fd(&self, shm_offset: u64, length: u64, host_fd: RawFd) -> Result<()> {
        self.window
            .place(shm_offset, length, host_fd, 0, true)
            .map_err(|err| {
                log::error!(
                    "SHM map_host_fd: mmap(shm_offset=0x{:x}, len=0x{:x}, fd={}, failed: {}",
                    shm_offset,
                    length,
                    host_fd,
                    err
                );
                DeviceError::Io(err)
            })
    }

    /// Tear down a host fd overlay from the SHM region, restoring memfd
    /// backing. MAP_FIXED replaces the old mapping atomically -- no window
    /// of invalid pages.
    pub fn unmap_host_fd(&self, offset: u64, length: u64) -> Result<()> {
        log::debug!(
            "SHM unmap_host_fd: restoring memfd at offset=0x{:x} len=0x{:x}",
            offset,
            length
        );
        self.window.restore(offset, length).map_err(|err| {
            log::error!(
                "SHM unmap_host_fd: memfd restore failed at offset=0x{:x}: {}",
                offset,
                err
            );
            DeviceError::Io(err)
        })?;
        log::debug!(
            "SHM unmap_host_fd: restored backing at offset=0x{:x} len=0x{:x}",
            offset,
            length
        );
        Ok(())
    }

    pub fn memfd_raw(&self) -> RawFd {
        self.window.memfd_raw()
    }

    /// The window's local backing.
    pub fn window(&self) -> Arc<Window> {
        self.window.clone()
    }

    pub fn uc_zone_offset(&self) -> u64 {
        self.uc.base
    }
    pub fn wc_zone_offset(&self) -> u64 {
        self.wc.base
    }
    pub fn wb_zone_offset(&self) -> u64 {
        self.wb.base
    }
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// The size of the largest zone: no one extent can be longer.
    pub fn largest_zone(&self) -> u64 {
        self.uc.size.max(self.wc.size).max(self.wb.size)
    }
}

const PAGE_SIZE: u64 = 4096;

/// `v` rounded up to a multiple of `align` (a power of two), or None when
/// that does not fit in a u64. `v` is often a guest's number: the release
/// profile aborts on overflow, so a plain `+` here would let one app take
/// the whole backend down.
fn align_up(v: u64, align: u64) -> Option<u64> {
    Some(v.checked_add(align - 1)? & !(align - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_cfg() -> ZoneConfig {
        ZoneConfig {
            uc_size: 4096 * 2,
            wc_size: 4096 * 4,
            wb_size: 4096 * 2,
            owner_percent: 50,
        }
    }

    fn small_alloc() -> ShmAllocator {
        ShmAllocator::new(small_cfg())
    }

    /// A guest's length near u64::MAX is refused, not rounded past the top
    /// of a u64: the release profile aborts on overflow, and the backend
    /// with it.
    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mappings")]
    fn a_length_near_u64_max_is_refused_not_overflowed() {
        assert_eq!(align_up(u64::MAX, PAGE_SIZE), None);
        assert_eq!(align_up(0xFFFF_FFFF_FFFF_F001, PAGE_SIZE), None);
        assert_eq!(
            align_up(0xFFFF_FFFF_FFFF_F000, PAGE_SIZE),
            Some(0xFFFF_FFFF_FFFF_F000)
        );
        let mut a = small_alloc();
        for len in [
            u64::MAX,
            u64::MAX - 4094,
            0xFFFF_FFFF_FFFF_F001,
            0xFFFF_FFFF_FFFF_F000,
            1 << 63,
            4096 * 4 + 1,
        ] {
            for pg in [
                PgprotKind::Uncached,
                PgprotKind::WriteCombine,
                PgprotKind::WriteBack,
            ] {
                assert!(a.alloc(len, pg).is_err(), "{len:#x} in {pg:?}");
            }
        }
        assert_eq!(a.free_bytes(), (4096 * 2, 4096 * 4, 4096 * 2));
        assert!(!a.wc.free_extent(a.wc.base, u64::MAX));
        assert!(!a.wc.free_extent(a.wc.base + 4096, u64::MAX - 8192));
        let huge = ShmRegion {
            offset: a.wc.base,
            length: u64::MAX,
            pgprot: PgprotKind::WriteCombine,
        };
        assert!(a.free(&huge).is_err());
        // The whole zone is still one extent.
        assert!(a.alloc(4096 * 4, PgprotKind::WriteCombine).is_ok());
    }

    /// One guest process maps at most half a zone, and the last eighth is
    /// kept for processes that hold at most a sixteenth: the one that took
    /// its half cannot take another's first mapping (B2).
    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mappings")]
    fn one_guest_process_cannot_map_the_whole_zone() {
        let mib = 1u64 << 20;
        let mut a = ShmAllocator::new(ZoneConfig {
            uc_size: 4096 * 16,
            wc_size: 256 * mib,
            wb_size: 4096 * 16,
            owner_percent: 50,
        });
        let p = |t: u32| Owner::Proc {
            tgid: t,
            start_ns: 1,
        };
        let wc = PgprotKind::WriteCombine;
        // A game's one large mapping: half the zone is fine, more is not.
        let big = a.alloc_for(128 * mib, wc, p(1)).unwrap();
        assert!(a.alloc_for(4096, wc, p(1)).is_err(), "past its half");
        assert_eq!(a.held_by(wc, p(1)), 128 * mib);
        // A second process takes what is left down to the reserve (32 MiB).
        let second = a.alloc_for(96 * mib, wc, p(2)).unwrap();
        assert!(
            a.alloc_for(4096, wc, p(2)).is_err(),
            "the reserve is not its"
        );
        // A third, holding nothing, still gets its first mapping, up to the
        // floor of 16 MiB.
        let third = a.alloc_for(16 * mib, wc, p(3)).unwrap();
        assert!(a.alloc_for(4096, wc, p(3)).is_err());
        // A guest that does not say is held to the zone alone.
        assert!(a.alloc_for(16 * mib, wc, Owner::Unknown).is_ok());
        // Freeing gives the share back.
        a.free(&big).unwrap();
        assert_eq!(a.held_by(wc, p(1)), 0);
        assert!(a.alloc_for(64 * mib, wc, p(1)).is_ok());
        a.free(&second).unwrap();
        a.free(&third).unwrap();
        assert_eq!((a.held_by(wc, p(2)), a.held_by(wc, p(3))), (0, 0));
    }

    #[test]
    fn memfd_is_valid() {
        let a = small_alloc();
        assert!(a.memfd_raw() >= 0);
        assert!(a.window().addr() != 0);
        assert_eq!(a.total_size(), 4096 * 8);
    }

    #[test]
    fn zones_dont_overlap() {
        let a = small_alloc();
        assert_eq!(a.uc.base + a.uc.size, a.wc.base);
        assert_eq!(a.wc.base + a.wc.size, a.wb.base);
    }

    #[test]
    fn alloc_correct_zone() {
        let mut a = small_alloc();
        let uc = a.alloc(100, PgprotKind::Uncached).unwrap();
        let wc = a.alloc(100, PgprotKind::WriteCombine).unwrap();
        let wb = a.alloc(100, PgprotKind::WriteBack).unwrap();

        assert_eq!(uc.offset, a.uc.base);
        assert_eq!(wc.offset, a.wc.base);
        assert_eq!(wb.offset, a.wb.base);
    }

    #[test]
    fn alloc_respects_page_alignment() {
        let mut a = small_alloc();
        let r1 = a.alloc(1, PgprotKind::WriteBack).unwrap();
        let r2 = a.alloc(1, PgprotKind::WriteBack).unwrap();
        assert_eq!(r2.offset - r1.offset, 4096);
    }

    #[test]
    fn zone_full_returns_error() {
        let mut a = small_alloc();
        a.alloc(4096 * 2, PgprotKind::Uncached).unwrap();
        assert!(a.alloc(1, PgprotKind::Uncached).is_err());
        assert!(a.alloc(4096, PgprotKind::WriteCombine).is_ok());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no memfd_create")]
    fn map_host_fd_with_memfd_fallback() {
        // Tests using the default memfd-backed base_ptr (no set_base_ptr call)
        let mut a = small_alloc();
        let region = a.alloc(4096, PgprotKind::WriteCombine).unwrap();

        let host_fd = crate::sys::fd::memfd(c"test-host-fd", libc::MFD_CLOEXEC).unwrap();
        crate::sys::fd::ftruncate(&host_fd, 4096).unwrap();
        crate::sys::mem::Mapping::shared(&host_fd, 4096, 0, true)
            .unwrap()
            .write(0, &[0x42]);

        a.map_host_fd(
            region.offset,
            4096,
            std::os::fd::AsRawFd::as_raw_fd(&host_fd),
        )
        .unwrap();

        assert_eq!(a.window().read(region.offset, 1), [0x42]);
    }
}

// ============================================================
// The shared window
// ============================================================

/// Places device memory where the guest can reach it.
///
/// This exists because the backend cannot do the placement itself. `MAP_FIXED`
/// rewrites the calling process's page tables and nothing else, so a mapping
/// made here would never appear in the memory slot the VMM registered -- the
/// guest would read the window's own empty pages and find no device. The
/// descriptor has to travel up to whoever owns that address space.
///
/// It is a trait for the reason every VMM concern in this crate is one: the
/// crate names no VMM. A transport implements it, and a backend without one
/// keeps its mappings to itself and says so.
pub trait WindowPlacer: Send {
    /// Put `len` bytes of `fd`, starting `fd_offset` bytes into it, at
    /// `shm_offset` within the window.
    ///
    /// `fd_offset` is zero for every RM mapping -- the descriptor names the
    /// mapping already, and the offset is a cookie RM chose rather than a
    /// position in a file. A DRM object is the exception: GEM_MAP_OFFSET hands
    /// out a file offset and the memory is only reachable by mapping the node
    /// there.
    fn place(
        &self,
        shm_offset: u64,
        len: u64,
        fd: RawFd,
        fd_offset: u64,
        writable: bool,
    ) -> Result<()>;

    /// Return a range to empty. Not an unmap: leaving a hole would let a later
    /// access reach no mapping at all in a range the memory slot still covers.
    fn withdraw(&self, shm_offset: u64, len: u64) -> Result<()>;

    /// Map `len` bytes of the UVM file `fd`, at its own offset `addr`, into
    /// the UVM aperture (shared memory region 2) at `aperture_offset`.
    ///
    /// Not a window placement: UVM maps a semaphore pool only at the host
    /// address equal to its offset (uvm.c:792), so the VMM maps it at `addr`
    /// in its own address space and gives that range a memory slot of its
    /// own in the aperture. What may be asked for is decided by `uvmmap.rs`.
    /// A placer with no aperture refuses.
    fn place_uvm(&self, aperture_offset: u64, len: u64, fd: RawFd, addr: u64) -> Result<()> {
        let _ = (aperture_offset, len, fd, addr);
        Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into())
    }

    /// Undo a `place_uvm`. The VMM removes the memory slot before the
    /// mapping, so the guest never has a slot over nothing.
    fn withdraw_uvm(&self, aperture_offset: u64, len: u64) -> Result<()> {
        let _ = (aperture_offset, len);
        Err(std::io::Error::from_raw_os_error(libc::ENOTSUP).into())
    }
}

/// Whether the host lets `len` bytes of `fd` at `fd_offset` be mapped
/// writable, found by asking the kernel rather than by knowing which objects
/// are read-only.
///
/// nvidia.ko decides at mmap time: without NV_PROTECT_WRITEABLE in the mapping
/// context it clears VM_WRITE and VM_MAYWRITE (nv-mmap.c:756-761) -- the
/// user-shared-data page (gpu_user_shared_data.c:417-418) and, for a
/// non-admin, the PTIMER and MC windows of BAR0 (osapi.c:2203-2226); nvidia-drm
/// does the same for a read-only GEM node (nvidia-drm-gem.c:292-299). The mmap
/// still succeeds, so a placement made writable produces a read-only VMA in the
/// VMM, and the first guest write reaches KVM as a write fault it cannot
/// resolve: KVM_RUN fails with EFAULT and the VM stops (kvm_main.c:2928-3024,
/// mmu.c:3547-3567). So: map it read-only here and ask for write; mprotect
/// answers EACCES exactly when VM_MAYWRITE is gone (mm/mprotect.c).
///
/// The probe mapping is the same kind of mapping the placement makes, of the
/// same length at the same offset, and it is gone before this returns: the
/// mapping context stays on the file (nv-mmap.c:540-552 only reads it) and
/// the page references it takes are dropped at munmap. Anything the probe
/// cannot tell -- the mmap itself failing -- answers "writable", which is what
/// every placement assumed before, and the placement then fails on its own.
pub fn host_mapping_writable(fd: RawFd, len: u64, fd_offset: u64) -> bool {
    // A length no mapping can have: the placement fails on its own.
    let Some(len) = align_up(len.max(1), PAGE_SIZE).and_then(|l| usize::try_from(l).ok()) else {
        return true;
    };
    crate::sys::mem::probe_writable(fd, len, fd_offset).unwrap_or(true)
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use std::os::fd::{AsRawFd, OwnedFd};

    fn memfd(len: u64) -> OwnedFd {
        let fd = crate::sys::fd::memfd(c"probe", libc::MFD_CLOEXEC).unwrap();
        crate::sys::fd::ftruncate(&fd, len).unwrap();
        fd
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no memfd_create")]
    fn a_file_opened_read_only_is_probed_read_only_and_a_writable_one_writable() {
        let rw = memfd(8192);
        assert!(host_mapping_writable(rw.as_raw_fd(), 5000, 0));
        let ro = crate::sys::fd::open_path(
            &format!("/proc/self/fd/{}", rw.as_raw_fd()),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
        .unwrap();
        assert!(!host_mapping_writable(ro.as_raw_fd(), 4096, 4096));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mappings")]
    fn a_file_that_cannot_be_mapped_at_all_is_left_to_the_placement() {
        let fd = crate::sys::fd::open(c"/dev/null", libc::O_RDONLY).unwrap();
        assert!(host_mapping_writable(fd.as_raw_fd(), 4096, 0));
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    const M: u64 = MIB;

    fn zones(c: &ZoneConfig) -> (u64, u64, u64) {
        (c.uc_size / M, c.wc_size / M, c.wb_size / M)
    }

    /// The default flags are the default window, exactly.
    #[test]
    fn the_default_window_is_todays_split() {
        let c = ZoneConfig::for_window(ZoneConfig::DEFAULT_MIB, 50, 0).unwrap();
        assert_eq!(c, ZoneConfig::default_1gib());
        assert_eq!(zones(&c), (32, 768, 224));
        assert_eq!(c.share(c.wc_size), Share::half(c.wc_size));
        // With compute beside it too.
        assert_eq!(
            ZoneConfig::for_window(1024, 50, crate::uvmmap::APERTURE_MAX).unwrap(),
            c
        );
    }

    /// UC stays at 32 MiB, WC and WB split the rest 24:7 as in the default;
    /// below the default all three shrink in proportion. Every size allowed
    /// gives whole 2 MiB zones that add up to it.
    #[test]
    fn the_zones_grow_mostly_write_combining() {
        let at = |mib| zones(&ZoneConfig::for_window(mib, 50, 0).unwrap());
        assert_eq!(at(256), (8, 192, 56));
        assert_eq!(at(512), (16, 384, 112));
        assert_eq!(at(1024), (32, 768, 224));
        assert_eq!(at(4096), (32, 3148, 916));
        assert_eq!(at(16384), (32, 12660, 3692));
        assert_eq!(at(65536), (32, 50714, 14790));
        for mib in (ZoneConfig::MIN_MIB..=65536).step_by(ZoneConfig::ALIGN_MIB as usize) {
            let c = ZoneConfig::for_window(mib, 50, 0).unwrap();
            assert!(c.zones_aligned(), "{mib}: {c:?}");
            assert_eq!(c.total(), mib * M, "{mib}");
            assert!(c.wc_size >= c.wb_size && c.wb_size >= c.uc_size, "{mib}");
        }
    }

    #[test]
    fn a_window_that_cannot_be_had_is_refused_with_the_reason() {
        let err = |mib, p, ap| ZoneConfig::for_window(mib, p, ap).unwrap_err();
        assert!(err(128, 50, 0).contains("at least 256"));
        assert!(err(1000, 50, 0).contains("multiple of 64"));
        assert!(err(0, 50, 0).contains("at least"));
        assert!(err(1024, 0, 0).contains("1-95"));
        assert!(err(1024, 96, 0).contains("1-95"));
        // crosvm's region cap: the window and the aperture together.
        assert!(ZoneConfig::for_window(65536, 50, 0).is_ok());
        let e = err(65536, 50, crate::uvmmap::APERTURE_MAX);
        assert!(
            e.contains("MAX_SHARED_MEMORY_REGION_SIZE") && e.contains("aperture"),
            "{e}"
        );
        assert!(ZoneConfig::for_window(64512, 50, crate::uvmmap::APERTURE_MAX).is_ok());
        assert!(err((u64::MAX / 2) & !63, 50, 0).contains("MAX_SHARED"));
    }

    /// A large share: one process takes nine tenths of a zone, and the
    /// others still have the rest, each up to the floor.
    #[test]
    #[cfg_attr(miri, ignore = "Miri has no memfd_create")]
    fn a_large_share_lets_one_process_take_most_of_a_zone() {
        let c = ZoneConfig::for_window(1024, 90, 0).unwrap();
        let mut a = ShmAllocator::new(c);
        let p = |t| Owner::Proc {
            tgid: t,
            start_ns: 1,
        };
        let wc = PgprotKind::WriteCombine;
        let s = c.share(c.wc_size);
        let big = s.per_owner & !(PAGE_SIZE - 1);
        a.alloc_for(big, wc, p(1)).unwrap();
        assert!(a.alloc_for(big / 16, wc, p(1)).is_err(), "past its share");
        let first = a.alloc_for(s.floor & !(PAGE_SIZE - 1), wc, p(2)).unwrap();
        assert!(a.alloc_for(PAGE_SIZE, wc, p(2)).is_err(), "past the floor");
        let (_, w, _) = a.peaks();
        assert_eq!(w.used, big + first.length);
        assert_eq!(w.one_process, big);
        assert_eq!(w.largest, big);
        assert_eq!((w.allocs, w.refused_share, w.refused_space), (2, 2, 0));
        let line = a.usage_summary();
        assert!(
            line.starts_with("window 1024 MiB, 90% per process: UC 32 MiB"),
            "{line}"
        );
        assert!(line.contains("WC 768 MiB: peak"), "{line}");
    }
}
