//! Active guest mappings.
//!
//! One entry per live `NV_ESC_RM_MAP_MEMORY`, keyed by the SHM offset that was
//! written back into `pLinearAddress`. The guest library stores that value and
//! echoes it in `NV_ESC_RM_UNMAP_MEMORY`, which gives an unambiguous lookup key
//! without handing the guest a host address.
//!
//! The entry owns the `ShmRegion`, because releasing a mapping means two things
//! that must not come apart: restoring the SHM backing, and returning the
//! extent to its zone. Doing only the first is what made the allocator leak.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use crate::shm::ShmRegion;

/// One live mapping.
#[derive(Debug, Clone, Copy)]
pub struct MmapEntry {
    /// The host's `pLinearAddress`, substituted back in on unmap.
    pub host_p_linear_address: u64,
    /// Length the guest asked for, before page rounding.
    pub shm_length: u64,
    pub h_client: u32,
    pub h_memory: u32,
    /// The guest handle whose host fd carries this mapping.
    ///
    /// A host fd is single-use for mapping: once it has carried one,
    /// `NV_ESC_RM_MAP_MEMORY` on it again is refused with NV_ERR_STATE_IN_USE,
    /// even after the unmap and free both succeed. The mapping therefore
    /// belongs to the fd for the fd's whole life, and closing the fd is a
    /// perfectly normal way to release it -- CUDA never unmaps at all, it just
    /// exits.
    pub map_fd_handle: u32,
    /// What to hand back to the SHM allocator when this mapping goes away.
    /// Its `pgprot` is the memory type the guest is told to map it with.
    pub region: ShmRegion,
    /// Whether the host lets it be written (see `shm::host_mapping_writable`).
    pub writable: bool,
    /// The id the first MMAP of it handed out, or 0 before then.
    ///
    /// The placement is made at RM_MAP_MEMORY and used to be released at
    /// RM_UNMAP_MEMORY, whatever the guest still had mapped: RM does not know
    /// about guest page tables, and an unmap does not reach the process that
    /// mapped (nv-mmap.c:443 keeps a native process's pages until its vma
    /// closes). The extent then went to the next mapping -- another process's
    /// -- while the first still had the range in its page tables. An id makes
    /// guest vmas count, so the extent outlives them.
    pub mapping_id: u32,
    /// MMAP replies that handed `mapping_id` out and have not been taken back.
    pub refs: u32,
}

/// Live mappings, indexed by SHM offset.
#[derive(Default)]
pub struct MmapContext {
    entries: HashMap<u64, MmapEntry>,
}

impl MmapContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, shm_offset: u64, entry: MmapEntry) {
        self.entries.insert(shm_offset, entry);
    }

    pub fn remove(&mut self, shm_offset: u64) -> Option<MmapEntry> {
        self.entries.remove(&shm_offset)
    }

    /// The mapping at a window offset.
    pub fn find_by_offset(&self, shm_offset: u64) -> Option<&MmapEntry> {
        self.entries.get(&shm_offset)
    }

    /// The mapping made on a given descriptor.
    ///
    /// This is the lookup the mmap path needs, and not `find_by_offset`. The
    /// guest driver sends `vma->vm_pgoff` as the offset, and the userspace
    /// library maps at offset 0 -- it does not quote the cookie written into
    /// pLinearAddress. What identifies the mapping is the descriptor the mmap
    /// arrives on, which works because a host fd is single-use for mapping, so
    /// at most one mapping exists per fd.
    pub fn find_by_fd_handle(&self, fd_handle: u32) -> Option<&MmapEntry> {
        self.entries.values().find(|e| e.map_fd_handle == fd_handle)
    }

    /// Take every mapping, leaving the table empty. Used at teardown, where a
    /// guest process that exited without unmapping is the normal case.
    pub fn drain(&mut self) -> Vec<MmapEntry> {
        self.entries.drain().map(|(_, e)| e).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The mapping made on a given descriptor, to update.
    pub fn find_by_fd_handle_mut(&mut self, fd_handle: u32) -> Option<&mut MmapEntry> {
        self.entries
            .values_mut()
            .find(|e| e.map_fd_handle == fd_handle)
    }

    /// The mapping an MMAP reply named `mapping_id`.
    pub fn find_by_mapping_id_mut(&mut self, mapping_id: u32) -> Option<&mut MmapEntry> {
        if mapping_id == 0 {
            return None;
        }
        self.entries
            .values_mut()
            .find(|e| e.mapping_id == mapping_id)
    }

    /// Whether an entry already answers to `mapping_id`.
    pub fn has_mapping_id(&self, mapping_id: u32) -> bool {
        mapping_id != 0 && self.entries.values().any(|e| e.mapping_id == mapping_id)
    }

    /// Take every mapping carried by one guest handle.
    ///
    /// Closing a device fd releases its mappings, and for some clients that is
    /// the only way they are ever released: a CUDA run makes 29 mappings and
    /// issues no unmap at all. Without this the extents survive until VM
    /// teardown, so each run permanently costs the write-combine zone ~68 MiB.
    pub fn take_for_fd(&mut self, fd_handle: u32) -> Vec<MmapEntry> {
        let keys: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.map_fd_handle == fd_handle)
            .map(|(k, _)| *k)
            .collect();
        keys.into_iter()
            .filter_map(|k| self.entries.remove(&k))
            .collect()
    }

    /// Whether this handle already carries a mapping.
    pub fn fd_has_mapping(&self, fd_handle: u32) -> bool {
        self.entries.values().any(|e| e.map_fd_handle == fd_handle)
    }

    /// Find the mapping for a given client/memory pair.
    ///
    /// `NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO` sends an address the guest knows,
    /// which never matches a host address, so the host `pLinearAddress` has to
    /// be recovered by identity instead.
    pub fn find_by_object(&self, h_client: u32, h_memory: u32) -> Option<&MmapEntry> {
        self.entries
            .values()
            .find(|e| e.h_client == h_client && e.h_memory == h_memory)
    }
}
