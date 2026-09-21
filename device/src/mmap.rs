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
    /// What to hand back to the SHM allocator when this mapping goes away.
    pub region: ShmRegion,
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
