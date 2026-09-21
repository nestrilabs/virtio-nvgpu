// crates/device/src/shm.rs

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;

use crate::error::{DeviceError, Result};

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
}

impl Zone {
    fn new(base: u64, size: u64) -> Self {
        let mut free = BTreeMap::new();
        if size > 0 {
            free.insert(0, size);
        }
        Self { base, size, free }
    }

    /// First-fit. Returns an absolute offset, or `None` if no extent fits.
    fn alloc(&mut self, length: u64) -> Option<u64> {
        let want = align_up(length, PAGE_SIZE);
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
        let want = align_up(length, PAGE_SIZE);
        if want == 0 || offset < self.base {
            return false;
        }
        let start = offset - self.base;
        if start + want > self.size {
            return false;
        }

        // Overlap with an existing free extent means this was freed already.
        if let Some((&ps, &pl)) = self.free.range(..=start).next_back() {
            if ps + pl > start {
                return false;
            }
        }
        if let Some((&ns, _)) = self.free.range(start..).next() {
            if start + want > ns {
                return false;
            }
        }

        let mut s = start;
        let mut l = want;

        // Coalesce with the extent below, if it ends exactly here.
        if let Some((&ps, &pl)) = self.free.range(..s).next_back() {
            if ps + pl == s {
                self.free.remove(&ps);
                s = ps;
                l += pl;
            }
        }
        // Coalesce with the extent above, if it starts exactly at our end.
        if let Some((&ns, &nl)) = self.free.range(s + l..).next() {
            if s + l == ns {
                self.free.remove(&ns);
                l += nl;
            }
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

pub struct ZoneConfig {
    pub uc_size: u64,
    pub wc_size: u64,
    pub wb_size: u64,
}

impl ZoneConfig {
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
    pub fn default_1gib() -> Self {
        Self {
            uc_size: 32 * 1024 * 1024,
            wc_size: 768 * 1024 * 1024,
            wb_size: 224 * 1024 * 1024,
        }
    }

    /// The original 256 MiB split. Too small for a single encode with any
    /// margin; kept only for tests that want a zone they can exhaust.
    pub fn default_256mib() -> Self {
        Self {
            uc_size: 4 * 1024 * 1024,
            wc_size: 128 * 1024 * 1024,
            wb_size: 124 * 1024 * 1024,
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

    /// Base pointer for MAP_FIXED operations.
    /// Initially points to the memfd mmap (self-owned fallback).
    /// Overridden to the guest memory HVA via set_base_ptr().
    base_ptr: *mut u8,

    /// Self-owned memfd mapping — used as fallback when no external
    /// base pointer is provided (e.g., unit tests).
    memfd: Option<OwnedFd>,
    memfd_ptr: *mut u8,
    memfd_size: u64,

    total_size: u64,
}

unsafe impl Send for ShmAllocator {}
unsafe impl Sync for ShmAllocator {}

impl ShmAllocator {
    pub fn new(cfg: ZoneConfig) -> Self {
        assert_eq!(cfg.uc_size % 4096, 0);
        assert_eq!(cfg.wc_size % 4096, 0);
        assert_eq!(cfg.wb_size % 4096, 0);

        let total = cfg.total();
        assert!(total > 0);

        // Create a memfd as fallback backing (used for tests and
        // before set_base_ptr is called).
        let name = CString::new("virtio-gpu-nv-shm").unwrap();
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(
            raw_fd >= 0,
            "memfd_create failed: {}",
            std::io::Error::last_os_error()
        );
        let memfd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        let ret = unsafe { libc::ftruncate(memfd.as_raw_fd(), total as libc::off_t) };
        assert_eq!(
            ret,
            0,
            "ftruncate failed: {}",
            std::io::Error::last_os_error()
        );

        let memfd_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                total as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                memfd.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            memfd_ptr,
            libc::MAP_FAILED,
            "mmap SHM BAR failed: {}",
            std::io::Error::last_os_error()
        );

        let uc_base = 0;
        let wc_base = cfg.uc_size;
        let wb_base = cfg.uc_size + cfg.wc_size;

        Self {
            uc: Zone::new(uc_base, cfg.uc_size),
            wc: Zone::new(wc_base, cfg.wc_size),
            wb: Zone::new(wb_base, cfg.wb_size),
            base_ptr: memfd_ptr as *mut u8,
            memfd: Some(memfd),
            memfd_ptr: memfd_ptr as *mut u8,
            memfd_size: total,
            total_size: total,
        }
    }

    pub fn with_default_zones() -> Self {
        Self::new(ZoneConfig::default_1gib())
    }

    /// Override the base pointer used for MAP_FIXED operations.
    pub fn set_base_ptr(&mut self, ptr: *mut u8) {
        log::info!(
            "ShmAllocator: base_ptr updated from {:?} to {:?}",
            self.base_ptr,
            ptr
        );
        self.base_ptr = ptr;
    }

    pub fn alloc(&mut self, length: u64, pgprot: PgprotKind) -> Result<ShmRegion> {
        let zone = match pgprot {
            PgprotKind::Uncached => &mut self.uc,
            PgprotKind::WriteCombine => &mut self.wc,
            PgprotKind::WriteBack => &mut self.wb,
        };

        match zone.alloc(length) {
            Some(offset) => Ok(ShmRegion {
                offset,
                length,
                pgprot,
            }),
            None => Err(DeviceError::Io(std::io::Error::new(
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
            ))),
        }
    }

    /// mmap a host fd into the SHM region at the given offset.
    /// Return a region to its zone and restore its SHM backing.
    ///
    /// Restoring the backing and reclaiming the extent must happen together.
    /// Doing only the first leaks the address range -- which is what the bump
    /// allocator did, and why a second NVENC encode in one guest ran the
    /// write-combine zone out of space.
    pub fn free(&mut self, region: &ShmRegion) -> Result<()> {
        let len = align_up(region.length, PAGE_SIZE);

        // Put the memfd back under this range before the extent can be handed
        // to another mapping; the guest keeps the whole window mapped, so the
        // range must never be left without backing.
        unsafe { self.unmap_host_fd(region.offset, len)? };

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
        Ok(())
    }

    /// Free bytes remaining in each zone, as `(uc, wc, wb)`.
    pub fn free_bytes(&self) -> (u64, u64, u64) {
        (self.uc.free_bytes(), self.wc.free_bytes(), self.wb.free_bytes())
    }

    /// Largest single allocation each zone could still satisfy.
    pub fn largest_free(&self) -> (u64, u64, u64) {
        (self.uc.largest_free(), self.wc.largest_free(), self.wb.largest_free())
    }

    pub fn map_host_fd(&self, shm_offset: u64, length: u64, host_fd: RawFd) -> Result<()> {
        let target = unsafe { self.base_ptr.add(shm_offset as usize) as *mut libc::c_void };

        let ptr = unsafe {
            libc::mmap(
                target,
                length as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                host_fd,
                0, // nvidia mmap handler uses context list, not offset
            )
        };
        if ptr == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            log::error!(
                "SHM map_host_fd: mmap(shm_offset=0x{:x}, len=0x{:x}, fd={}, failed: {}",
                shm_offset,
                length,
                host_fd,
                err
            );
            return Err(DeviceError::Io(err));
        }
        Ok(())
    }

    /// Tear down a host fd overlay from the SHM region, restoring memfd backing.
    pub unsafe fn unmap_host_fd(&self, offset: u64, length: u64) -> Result<()> {
        let target = unsafe { self.base_ptr.add(offset as usize) as *mut libc::c_void };

        log::debug!(
            "SHM unmap_host_fd: restoring memfd at offset=0x{:x} len=0x{:x}",
            offset,
            length
        );

        let memfd_raw = self.memfd_raw();
        if memfd_raw >= 0 {
            // Overlay the memfd back onto this range, replacing the host fd mapping.
            // MAP_FIXED atomically replaces the old mapping — no window of invalid pages.
            let ptr = unsafe {
                libc::mmap(
                    target,
                    length as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    memfd_raw,
                    offset as libc::off_t,
                )
            };
            if ptr == libc::MAP_FAILED {
                let err = std::io::Error::last_os_error();
                log::error!(
                    "SHM unmap_host_fd: memfd restore failed at offset=0x{:x}: {}",
                    offset,
                    err
                );
                return Err(DeviceError::Io(err));
            }
        } else {
            // No memfd — this shouldn't happen in practice, but handle it
            // by just unmapping. The guest will see a hole (SIGBUS on access).
            log::warn!("SHM unmap_host_fd: no memfd, falling back to munmap");
            let ret = unsafe { libc::munmap(target, length as usize) };
            if ret != 0 {
                let err = std::io::Error::last_os_error();
                log::error!("SHM unmap_host_fd: munmap failed: {}", err);
                return Err(DeviceError::Io(err));
            }
        }

        log::info!(
            "SHM unmap_host_fd: restored backing at offset=0x{:x} len=0x{:x}",
            offset,
            length
        );
        Ok(())
    }

    pub fn memfd_raw(&self) -> RawFd {
        self.memfd.as_ref().map_or(-1, |fd| fd.as_raw_fd())
    }

    pub fn base_ptr(&self) -> *mut u8 {
        self.base_ptr
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
}

impl Drop for ShmAllocator {
    fn drop(&mut self) {
        // Only unmap the memfd mapping, not the guest memory.
        if !self.memfd_ptr.is_null() {
            unsafe {
                libc::munmap(
                    self.memfd_ptr as *mut libc::c_void,
                    self.memfd_size as usize,
                );
            }
            self.memfd_ptr = ptr::null_mut();
        }
        // OwnedFd drops the memfd automatically.
    }
}

const PAGE_SIZE: u64 = 4096;

fn align_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_cfg() -> ZoneConfig {
        ZoneConfig {
            uc_size: 4096 * 2,
            wc_size: 4096 * 4,
            wb_size: 4096 * 2,
        }
    }

    fn small_alloc() -> ShmAllocator {
        ShmAllocator::new(small_cfg())
    }

    #[test]
    fn memfd_is_valid() {
        let a = small_alloc();
        assert!(a.memfd_raw() >= 0);
        assert!(!a.base_ptr().is_null());
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
    fn set_base_ptr_changes_target() {
        let mut a = small_alloc();
        let original = a.base_ptr();
        let fake_ptr = 0xDEAD_0000 as *mut u8;
        a.set_base_ptr(fake_ptr);
        assert_eq!(a.base_ptr(), fake_ptr);
        assert_ne!(a.base_ptr(), original);
    }

    #[test]
    fn map_host_fd_with_memfd_fallback() {
        // Tests using the default memfd-backed base_ptr (no set_base_ptr call)
        let mut a = small_alloc();
        let region = a.alloc(4096, PgprotKind::WriteCombine).unwrap();

        let name = CString::new("test-host-fd").unwrap();
        let host_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(host_fd >= 0);
        unsafe {
            libc::ftruncate(host_fd, 4096);
            let tmp = libc::mmap(
                ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                host_fd,
                0,
            );
            assert_ne!(tmp, libc::MAP_FAILED);
            *(tmp as *mut u8) = 0x42;
            libc::munmap(tmp, 4096);
        }

        a.map_host_fd(region.offset, 4096, host_fd).unwrap();

        unsafe {
            let val = *a.base_ptr().add(region.offset as usize);
            assert_eq!(val, 0x42);
        }

        unsafe { libc::close(host_fd) };
    }
}
