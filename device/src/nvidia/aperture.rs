//! The UVM aperture: where a CUDA semaphore pool reaches the guest.
//!
//! Creating a CUDA context makes a UVM semaphore pool at an address the
//! caller chose, then maps the UVM file there at an offset equal to that
//! address. UVM takes the mapping only at the host address equal to the
//! offset, and only for the pool's own range. The shared window is none of
//! that: its host address is wherever the VMM reserved it. So a pool is never
//! placed in the window. The VMM maps the file at the pool's own address in
//! its own address space and gives that range a memory slot inside a second
//! region, the aperture, at an offset chosen here. The guest maps its vma from
//! there.
//!
//! The VMM rather than this process because the slot has to describe the
//! VMM's address space, and UVM lets it: the backend initialises every UVM
//! file in multi-process sharing mode (see `uvm.rs`), which lifts the rule
//! that only the initialising process may map it.

use super::*;

/// Size of the aperture region the VMM offers, shm id 2.
pub const APERTURE_LEN: u64 = 1 << 30;
/// Every pool starts on a 2 MiB boundary of the aperture, so a slot can be
/// backed by huge pages where the host has them.
pub const APERTURE_ALIGN: u64 = 2 << 20;
/// Largest single pool. CUDA makes them at a few MiB.
pub const POOL_MAX_LEN: u64 = 64 << 20;
/// Pools and bytes across one VM.
pub const MAX_POOLS: usize = 64;
pub const MAX_POOL_BYTES: u64 = 256 << 20;
/// The band of host addresses a pool may name. Below 4 GiB is where a VMM's
/// own small mappings live; above 32 TiB is past what the VMM leaves free. The
/// VMM checks the same band and maps without replacing anything, so this is
/// the first of two checks, not the only one.
pub const HVA_MIN: u64 = 4 << 30;
pub const HVA_MAX: u64 = 32 << 40;

/// One pool placed in the aperture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pool {
    /// The UVM file it was mapped from.
    pub handle: u64,
    /// The pool's address, which is also the file offset.
    pub addr: u64,
    pub offset: u64,
    pub len: u64,
}

/// Offsets in the aperture, by mapping id.
#[derive(Default)]
pub struct Aperture {
    pools: std::collections::BTreeMap<u32, Pool>,
}

impl Aperture {
    /// Check a pool and find it room, without placing it.
    pub fn admit(&self, handle: u64, addr: u64, len: u64) -> std::result::Result<Pool, i32> {
        let page = 4096;
        if len == 0
            || len % page != 0
            || len > POOL_MAX_LEN
            || addr % page != 0
            || addr < HVA_MIN
            || addr.checked_add(len).is_none_or(|end| end > HVA_MAX)
        {
            return Err(libc::EINVAL);
        }
        let used: u64 = self.pools.values().map(|p| p.len).sum();
        if self.pools.len() >= MAX_POOLS || used + len > MAX_POOL_BYTES {
            return Err(libc::ENOSPC);
        }
        // The same host range twice would be a second mapping over the first,
        // which the VMM refuses anyway; saying so here names the cause.
        if self
            .pools
            .values()
            .any(|p| p.addr < addr + len && addr < p.addr + p.len)
        {
            return Err(libc::EEXIST);
        }
        let mut taken: Vec<(u64, u64)> = self.pools.values().map(|p| (p.offset, p.len)).collect();
        taken.sort_unstable();
        let mut at = 0u64;
        for (o, l) in taken {
            if at + len <= o {
                break;
            }
            at = (o + l).div_ceil(APERTURE_ALIGN) * APERTURE_ALIGN;
        }
        if at + len > APERTURE_LEN {
            return Err(libc::ENOSPC);
        }
        Ok(Pool {
            handle,
            addr,
            offset: at,
            len,
        })
    }

    pub fn insert(&mut self, id: u32, pool: Pool) {
        self.pools.insert(id, pool);
    }

    pub fn remove(&mut self, id: u32) -> Option<Pool> {
        self.pools.remove(&id)
    }

    pub fn take_for_handle(&mut self, handle: u64) -> Vec<(u32, Pool)> {
        let ids: Vec<u32> = self
            .pools
            .iter()
            .filter(|(_, p)| p.handle == handle)
            .map(|(&id, _)| id)
            .collect();
        ids.into_iter()
            .filter_map(|id| self.pools.remove(&id).map(|p| (id, p)))
            .collect()
    }

    pub fn take_all(&mut self) -> Vec<(u32, Pool)> {
        std::mem::take(&mut self.pools).into_iter().collect()
    }
}

impl NvidiaBackend {
    /// Serve an mmap on a UVM file: a semaphore pool, placed in the aperture.
    pub(super) fn map_uvm_pool(&mut self, size: u64, offset: u64, resp_buf: &mut [u8]) -> usize {
        let handle = self.current_handle as u64;
        let host_fd = match self.handles.get_raw(handle) {
            Ok(fd) => fd,
            Err(_) => return self.write_error_resp(resp_buf, Status::BadHandle, 0, libc::ENOENT),
        };
        let pool = match self.aperture.admit(handle, offset, size) {
            Ok(p) => p,
            Err(errno) => {
                log::warn!(
                    "UVM handle {handle}: refusing a pool at {offset:#x}+{size:#x} (errno {errno})"
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, errno);
            }
        };
        let Some(window) = self.window.as_ref() else {
            log::warn!("UVM handle {handle}: no transport to place a pool through");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::ENOTSUP);
        };
        if let Err(e) = window.place_pool(pool.offset, pool.len, host_fd, pool.addr) {
            log::warn!(
                "UVM handle {handle}: the VMM would not place the pool at {:#x}+{:#x}: {e}",
                pool.addr,
                pool.len
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EINVAL);
        }
        let id = self.next_mapping_id;
        self.next_mapping_id = self.next_mapping_id.wrapping_add(1).max(1);
        self.aperture.insert(id, pool);
        log::debug!(
            "UVM handle {handle}: pool {:#x}+{:#x} at aperture offset {:#x}, id {id}",
            pool.addr,
            pool.len,
            pool.offset
        );

        let need = size_of::<MsgHeader>() + size_of::<MmapResp>();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
        }
        let mut off = self.write_hdr(resp_buf, self.current_handle, 0);
        off += write_struct(
            &mut resp_buf[off..],
            &MmapResp {
                guest_phys_addr: pool.offset,
                size: pool.len,
                mapping_id: id,
                padding: 0,
            },
        );
        off
    }

    /// Take a pool out of the aperture: the VMM drops the slot, then its
    /// mapping, so the guest never has a slot over nothing.
    pub(super) fn unmap_uvm_pool(&mut self, id: u32, pool: Pool) {
        if let Some(window) = self.window.as_ref() {
            if let Err(e) = window.withdraw_pool(pool.offset, pool.len) {
                log::warn!(
                    "UVM pool {id} at {:#x}+{:#x}: the VMM would not give it back: {e}",
                    pool.addr,
                    pool.len
                );
            }
        }
    }

    /// Every pool a UVM file still has placed, as it closes.
    pub(super) fn drop_pools_for_file(&mut self, handle: u64) {
        for (id, pool) in self.aperture.take_for_handle(handle) {
            self.unmap_uvm_pool(id, pool);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const HVA: u64 = 8 << 30;

    #[test]
    fn pools_go_on_2mib_boundaries_and_reuse_freed_room() {
        let mut ap = Aperture::default();
        let a = ap.admit(1, HVA, 4096).unwrap();
        assert_eq!(a.offset, 0);
        ap.insert(1, a);
        let b = ap.admit(1, HVA + 4 * MIB, 3 * MIB).unwrap();
        assert_eq!(b.offset, 2 * MIB);
        ap.insert(2, b);
        let c = ap.admit(1, HVA + 16 * MIB, MIB).unwrap();
        assert_eq!(c.offset, 6 * MIB);
        ap.remove(1);
        assert_eq!(ap.admit(1, HVA + 32 * MIB, MIB).unwrap().offset, 0);
    }

    #[test]
    fn a_pool_out_of_bounds_is_refused() {
        let ap = Aperture::default();
        for (addr, len) in [
            (HVA, 0),
            (HVA, 4097),
            (HVA + 1, 4096),
            (HVA, POOL_MAX_LEN + 4096),
            (HVA_MIN - 4096, 4096),
            (HVA_MAX - 4096, 8192),
            (u64::MAX & !4095, 4096),
        ] {
            assert_eq!(
                ap.admit(1, addr, len),
                Err(libc::EINVAL),
                "{addr:#x}+{len:#x}"
            );
        }
    }

    #[test]
    fn overlapping_and_too_many_pools_are_refused() {
        let mut ap = Aperture::default();
        ap.insert(1, ap.admit(1, HVA, 2 * MIB).unwrap());
        assert_eq!(ap.admit(1, HVA + MIB, MIB), Err(libc::EEXIST));
        for i in 1..4u32 {
            let p = ap.admit(1, HVA + i as u64 * 64 * MIB, 64 * MIB).unwrap();
            ap.insert(i + 1, p);
        }
        assert_eq!(ap.admit(1, HVA + 512 * MIB, 64 * MIB), Err(libc::ENOSPC));
    }

    #[test]
    fn closing_a_file_takes_only_its_pools() {
        let mut ap = Aperture::default();
        ap.insert(1, ap.admit(1, HVA, MIB).unwrap());
        ap.insert(2, ap.admit(2, HVA + 8 * MIB, MIB).unwrap());
        let gone = ap.take_for_handle(1);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].0, 1);
        assert_eq!(ap.take_all().len(), 1);
    }
}
