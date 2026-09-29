// SPDX-License-Identifier: Apache-2.0
//! Placing device memory where the guest can address it: MMAP and MUNMAP,
//! the window extents they take and give back, and the memory type each is
//! mapped with.

#![forbid(unsafe_code)]

use super::*;

/// A placement the guest can hand back, and everything needed to undo it.
///
/// This record is the extent's only owner. It used to share it with an
/// `active_maps` entry keyed to the handle that made it, and closing that
/// handle freed the extent there -- without withdrawing it from the window and
/// without removing this record. A GEM proxy that outlived its owner file (a
/// compositor holding a client's buffer after the client exited) kept the old
/// placement mapped while the extent went to someone else's object, and its
/// eventual MUNMAP withdrew and freed the *new* placement: two objects on one
/// extent, then none. Now a close leaves the placement alone, and the extent
/// is withdrawn and freed exactly once, when the last reference goes: the last
/// MUNMAP of its id, or a session reset.
///
/// RM mappings end up here too, once RM_UNMAP_MEMORY or the close of their
/// file has ended them on the host while a guest vma still maps the id: the
/// extent must not go to another mapping until those vmas are gone.
pub(crate) struct LiveMap {
    /// The `dri_maps` entry that finds it, for a DRM object; none for an RM
    /// mapping, which nothing maps again once it is here.
    pub(super) key: Option<(u32, u64)>,
    pub(super) region: crate::shm::ShmRegion,
    pub(super) length: u64,
    /// MMAP replies that handed this id out and have not been taken back.
    pub(super) refs: u32,
    /// Whether the host mapping can be written (`shm::host_mapping_writable`).
    pub(super) writable: bool,
}

impl NvidiaBackend {
    /// Withdraw a placement from the window and return its extent. Called
    /// exactly once per extent, by whichever record owns it.
    pub(super) fn release_extent(
        &mut self,
        region: &crate::shm::ShmRegion,
        length: u64,
        why: &str,
    ) {
        // Emptied rather than unmapped: a hole would leave the memory slot
        // covering a range that reaches no mapping at all, and a stray access
        // there faults the VMM rather than the guest.
        if let Some(window) = self.window.as_ref()
            && let Err(e) = window.withdraw(region.offset, length)
        {
            log::warn!(
                "{why}: the window would not give back {:#x}+{length:#x}: {e}",
                region.offset
            );
        }
        if let Err(e) = self.shm.free(region) {
            log::warn!("{why}: freeing window region {:#x}: {e}", region.offset);
        }
    }

    /// Ask the VMM to take a UVM pool out of the aperture: its memory slot,
    /// then its mapping (the VMM's order). A failure is logged and the
    /// aperture space is ours again anyway: the VMM is gone, or has already
    /// dropped it.
    pub(super) fn withdraw_uvm(&self, (off, len): crate::uvmmap::Withdraw, why: &str) {
        match self.window.as_ref().map(|w| w.withdraw_uvm(off, len)) {
            Some(Ok(())) => log::info!("{why}: UVM pool at aperture {off:#x}+{len:#x} withdrawn"),
            Some(Err(e)) => log::warn!(
                "{why}: the VMM would not withdraw the UVM pool at aperture {off:#x}+{len:#x}: {e}"
            ),
            None => log::warn!("{why}: no window to withdraw aperture {off:#x}+{len:#x} from"),
        }
    }

    /// Place a mapping the guest asked for into the shared window.
    ///
    /// The guest quotes the cookie a previous `RM_MAP_MEMORY` wrote into
    /// pLinearAddress, which is the offset within the shared window, so this
    /// only has to find the region that cookie belongs to and hand back where
    /// it sits.
    pub(super) fn handle_mmap(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        let Some(req) = pod::read::<MmapReq>(payload, 0) else {
            return self.write_error(resp_buf, libc::EINVAL);
        };

        // Only files that have device memory to place: the NVIDIA devices and
        // DRM nodes. A sync_file, a dmabuf or a compositor's memfd reached
        // through here would put host memory in the window by a path nobody
        // designed for it.
        match self.current_kind() {
            Some(
                HandleKind::Dev(_)
                | HandleKind::DriRender(_)
                | HandleKind::DrmCard(_)
                | HandleKind::DrmLease(_),
            )
            | None => {}
            Some(k) => {
                log::warn!("mmap on handle {} ({k:?}) refused", self.current_handle);
                return self.write_error(resp_buf, libc::EPERM);
            }
        }

        // A UVM file's mapping is a semaphore pool, which UVM maps only at the
        // host address equal to its offset -- never in the window. Sent there,
        // the VMM's mmap of it failed and took the window's request channel
        // down with it.
        if matches!(
            self.current_kind(),
            Some(HandleKind::Dev(DeviceKind::Uvm | DeviceKind::UvmTools))
        ) {
            return self.uvm_mmap(&req, resp_buf);
        }

        // A DRM node never takes the recorded path. Its bookkeeping is keyed by
        // the file, and one open of a node holds every object a client ever
        // allocates -- so the second object's mmap would find the first one's
        // entry and hand back the first one's memory. The objects are told
        // apart by the offset, and that is what the path below keys on.
        if self.current_kind().and_then(HandleKind::index).is_some() {
            return self.map_unrecorded(req.size, req.offset, resp_buf);
        }

        let entry = match self.active_maps.find_by_fd_handle(self.current_handle) {
            Some(e) => *e,
            None => {
                // Bookkeeping here records what RM_MAP_MEMORY armed, and that
                // is not the only ioctl that arms a mapping: NV_ESC_RM_ALLOC_MEMORY
                // names a file in the same way and arms it too, which is what
                // the descriptor at the end of its parameters is for. Measured
                // against a host run, the second 4 KiB mapping of the control
                // device is armed that way, and refusing it here is the guest
                // seeing a failed mmap where the host sees a mapping.
                //
                // Which ioctl armed it is the driver's business, not ours. The
                // file either has a mapping waiting on it, in which case
                // mapping it into the window succeeds, or it has not, in which
                // case the kernel says so -- and that answer is better than our
                // records, because the driver is the one keeping them.
                return self.map_unrecorded(req.size, req.offset, resp_buf);
            }
        };

        // What goes back is the offset within the window, not a guest physical
        // address. The backend does not know where the window sits -- the bus
        // assigns that, and this crate names no VMM -- but the guest driver
        // does, because it reads the window's address out of its own device.
        // It adds the two.
        let (offset, length) = (entry.region.offset, entry.region.length);
        // A mapping is rarely a whole number of pages -- the usermode aperture
        // is 0xc70 bytes -- and mmap always covers whole pages, so the guest
        // asking for more than the mapping holds is the normal case and not an
        // overrun. What must not happen is a request past the page the mapping
        // ends in.
        let page = 4096u64;
        let mapped_pages = length.div_ceil(page) * page;
        if req.size > mapped_pages {
            log::warn!(
                "mmap: guest asked for {:#x} bytes of a {length:#x}-byte mapping at {offset:#x}",
                req.size
            );
            return self.write_error(resp_buf, libc::EINVAL);
        }

        // An id the guest's vmas count on, the same one for every MMAP of this
        // mapping: RM_UNMAP_MEMORY then leaves the extent alone until the last
        // of them is gone (see `MmapEntry::mapping_id`).
        let id = if entry.mapping_id != 0 {
            entry.mapping_id
        } else {
            self.next_mapping_id()
        };
        if let Some(e) = self.active_maps.find_by_fd_handle_mut(self.current_handle) {
            e.mapping_id = id;
            e.refs += 1;
        }

        log::debug!("mmap: window offset {offset:#x}+{length:#x}, id {id}");
        self.write_mmap_resp(
            resp_buf,
            offset,
            mapped_pages,
            id,
            entry.region.pgprot,
            entry.writable,
        )
    }

    /// MMAP of a UVM file: one of its semaphore pools, exactly, placed in
    /// the UVM aperture at the pool's own host address (uvmmap.rs).
    pub(super) fn uvm_mmap(&mut self, req: &MmapReq, resp_buf: &mut [u8]) -> usize {
        let handle = self.current_handle;
        if self.current_kind() == Some(HandleKind::Dev(DeviceKind::UvmTools)) {
            return self.write_error(resp_buf, libc::EPERM);
        }
        if !self.session.v2 || self.window.is_none() {
            return self.write_error(resp_buf, libc::EINVAL);
        }
        let Ok(host_fd) = self.handles.get_raw(handle) else {
            return self.write_error(resp_buf, libc::ENOENT);
        };
        // Before anything is placed: a placement whose reply cannot be
        // written would hold a reference no guest mapping will ever give back.
        if resp_buf.len() < size_of::<MsgHeader>() + size_of::<MmapResp>() {
            return self.write_error(resp_buf, libc::ENOSPC);
        }
        let (base, len) = (req.offset, req.size);
        self.uvm_maps.set_owner(handle, self.handles.owner(handle));
        let plan = match self.uvm_maps.plan_mmap(handle, base, len, req.prot) {
            Ok(p) => p,
            Err(errno) => {
                log::debug!(
                    "mmap of UVM handle {handle} at {base:#x}+{len:#x} (prot {}) refused: {}",
                    req.prot,
                    std::io::Error::from_raw_os_error(errno)
                );
                return self.write_error(resp_buf, errno);
            }
        };
        let (off, id) = match plan {
            crate::uvmmap::MmapPlan::Existing(p) => {
                self.uvm_maps.add_ref(handle, base);
                (p.aperture_off, p.mapping_id)
            }
            crate::uvmmap::MmapPlan::New { aperture_off } => {
                let id = self.next_mapping_id();
                let placed = self.window.as_ref().expect("checked above").place_uvm(
                    aperture_off,
                    len,
                    host_fd,
                    base,
                );
                if let Err(e) = placed {
                    self.uvm_maps.abort(aperture_off, len);
                    log::warn!(
                        "mmap of UVM handle {handle}: the VMM would not map {base:#x}+{len:#x} \
                         at aperture {aperture_off:#x}: {e}"
                    );
                    return self.write_error(resp_buf, libc::ENOMEM);
                }
                self.uvm_maps.commit(handle, base, aperture_off, id);
                log::debug!(
                    "mmap of UVM handle {handle}: pool {base:#x}+{len:#x} at aperture \
                     {aperture_off:#x}, id {id}"
                );
                (aperture_off, id)
            }
        };
        let mut n = self.write_hdr(resp_buf, handle, 0);
        n += write_struct(
            &mut resp_buf[n..],
            &MmapResp {
                guest_phys_addr: off,
                size: len,
                mapping_id: id,
                caching: MMAP_CACHE_WB,
                flags: MMAP_F_UVM_APERTURE,
                reserved: 0,
            },
        );
        n
    }

    /// An MMAP reply. `pgprot` is the zone the placement is in, which is the
    /// memory type the host maps it with; a v2 guest maps it the same way,
    /// and a v1 guest, which reads nothing there, keeps write-combining.
    pub(super) fn write_mmap_resp(
        &self,
        resp_buf: &mut [u8],
        offset: u64,
        size: u64,
        id: u32,
        pgprot: crate::shm::PgprotKind,
        writable: bool,
    ) -> usize {
        use crate::shm::PgprotKind;
        let need = size_of::<MsgHeader>() + size_of::<MmapResp>();
        if resp_buf.len() < need {
            return self.write_error(resp_buf, libc::ENOSPC);
        }
        let (caching, flags) = if self.session.v2 {
            let c = match pgprot {
                PgprotKind::WriteBack => MMAP_CACHE_WB,
                PgprotKind::WriteCombine => MMAP_CACHE_WC,
                PgprotKind::Uncached => MMAP_CACHE_UC,
            };
            (c, if writable { 0 } else { MMAP_F_READ_ONLY })
        } else {
            (MMAP_CACHE_DEFAULT, 0)
        };
        let mut off = self.write_hdr(resp_buf, self.current_handle, 0);
        off += write_struct(
            &mut resp_buf[off..],
            &MmapResp {
                guest_phys_addr: offset,
                size,
                mapping_id: id,
                caching,
                flags,
                reserved: 0,
            },
        );
        off
    }

    /// A window extent in the zone for `want`. A write-back request that does
    /// not fit falls back to write-combining, which is always a correct way to
    /// map what the host maps write-back (only slower to read); nothing else
    /// falls back, least of all registers, which must stay uncached.
    ///
    /// The extent is charged to `owner`, which holds at most its share of a
    /// zone (quota.rs, B2).
    pub(super) fn alloc_zone(
        &mut self,
        length: u64,
        want: crate::shm::PgprotKind,
        owner: crate::quota::Owner,
    ) -> Result<crate::shm::ShmRegion> {
        use crate::shm::PgprotKind;
        match self.shm.alloc_for(length, want, owner) {
            Err(e) if want == PgprotKind::WriteBack => {
                log::warn!("write-back zone: {e}; placing {length:#x} bytes write-combining");
                self.shm.alloc_for(length, PgprotKind::WriteCombine, owner)
            }
            r => r,
        }
    }

    /// How the host maps what an RM_MAP_MEMORY of `h_client`'s `h_memory`
    /// armed, from the flags RM returned and our records (M-1).
    ///
    /// The caching type in the flags means something only for video memory:
    /// the escape resets it to DEFAULT before RM sees it (escape.c:600-601),
    /// and RM writes a real one back only when it maps through BAR1, marking
    /// the mapping REFLECTED (mapping_cpu.c:586-591). System memory comes back
    /// DIRECT with DEFAULT, and the host maps it with the allocation's own
    /// type (nv-mmap.c:727-729), which is in our records; registers (a
    /// usermode doorbell) come back with MAPPING as the caller left it and are
    /// mapped UC whatever was asked (nv-mmap.c:589-596).
    pub(super) fn rm_mapping_pgprot(
        &self,
        flags: u32,
        h_client: u32,
        h_memory: u32,
    ) -> crate::shm::PgprotKind {
        use crate::rmmem::Mem;
        use crate::shm::PgprotKind;
        const MAPPING_SHIFT: u32 = 15; // NVOS33_FLAGS_MAPPING 16:15
        const MAPPING_DIRECT: u32 = 1;
        const MAPPING_REFLECTED: u32 = 2;
        const CACHING_TYPE_SHIFT: u32 = 23; // NVOS33_FLAGS_CACHING_TYPE 25:23
        const CACHED: u32 = 0;
        const UNCACHED: u32 = 1;
        const WRITECOMBINED: u32 = 2;
        const WRITEBACK: u32 = 5;
        const UNCACHED_WEAK: u32 = 7;

        let mem = self.rmmem.lookup(h_client, h_memory);
        match ((flags >> MAPPING_SHIFT) & 3, mem) {
            (_, Some(Mem::Regmem)) => PgprotKind::Uncached,
            (MAPPING_REFLECTED, _) => match (flags >> CACHING_TYPE_SHIFT) & 7 {
                CACHED | WRITEBACK => PgprotKind::WriteBack,
                WRITECOMBINED => PgprotKind::WriteCombine,
                UNCACHED | UNCACHED_WEAK => PgprotKind::Uncached,
                // RM always writes a real type for BAR1; DEFAULT means it did
                // not map one, and the safe type for what it did map is UC.
                _ => PgprotKind::Uncached,
            },
            (_, Some(m)) => m.pgprot(),
            // System memory this backend never saw allocated: the old guess.
            (MAPPING_DIRECT, None) => PgprotKind::WriteCombine,
            _ => PgprotKind::Uncached,
        }
    }

    /// The memory type an RM_MAP_MEMORY of `h_client`'s `h_memory` will most
    /// likely be mapped with, known before RM answers: what
    /// `rm_mapping_pgprot` makes of the answer RM gives for it. Registers
    /// and system memory we saw allocated are in our records; anything
    /// else is taken to be video memory, which RM maps through BAR1
    /// write-combined. A wrong guess costs a second allocation, not a
    /// wrong type: the reply decides.
    pub(super) fn rm_mapping_pgprot_before(
        &self,
        h_client: u32,
        h_memory: u32,
    ) -> crate::shm::PgprotKind {
        match self.rmmem.lookup(h_client, h_memory) {
            Some(m) => m.pgprot(),
            None => crate::shm::PgprotKind::WriteCombine,
        }
    }

    /// Serve an mmap on a file this backend has no record of arming.
    ///
    /// The window placement and the reply are the same as the recorded path;
    /// only the source of the length differs — the guest's request, since there
    /// is no stored region to take it from.
    pub(super) fn map_unrecorded(&mut self, size: u64, offset: u64, resp_buf: &mut [u8]) -> usize {
        let handle = self.current_handle;
        let host_fd = match self.handles.get_raw(handle) {
            Ok(fd) => fd,
            Err(_) => return self.write_error(resp_buf, libc::ENOENT),
        };

        // Caching follows the device: a GPU device carries the card's own
        // memory, which the host maps write-combined through BAR1, and so does
        // a DRM node (drm_gem_mmap_obj, drm_gem.c:1250-1252, for every
        // nvidia-drm object). The control device carries system memory, armed
        // by an NV_ESC_RM_ALLOC_MEMORY whose coherency is in our records; one
        // we did not see is taken to be write-back only when this backend
        // makes guest system memory coherent, and write-combining otherwise,
        // which is safe for any system memory.
        let kind = self.current_kind();
        let drm = kind.and_then(HandleKind::index).is_some();
        let pgprot = match kind {
            Some(HandleKind::Dev(DeviceKind::Gpu(_))) => crate::shm::PgprotKind::WriteCombine,
            _ if drm => crate::shm::PgprotKind::WriteCombine,
            _ => match self.rmmem.armed(handle) {
                Some(m) => m.pgprot(),
                None if self.rmmem.coherent() => crate::shm::PgprotKind::WriteBack,
                None => crate::shm::PgprotKind::WriteCombine,
            },
        };

        // On a DRM node the guest's offset is a real position in the file --
        // GEM_MAP_OFFSET issued it, on this very descriptor -- and the object's
        // memory is reachable nowhere else. Everywhere else the offset is a
        // cookie RM chose, which names no position at all, and mapping the file
        // there would either fail or land on unrelated memory.
        let fd_offset = if drm { offset } else { 0 };

        // The same object mapped twice is the same memory: hand back the
        // placement that is already there rather than a second copy of it,
        // and count the reference, so the first MUNMAP does not pull it from
        // under the second user.
        if let Some(&id) = self.dri_maps.get(&(handle, fd_offset))
            && let Some(live) = self.live_maps.get_mut(&id)
        {
            // No more than the placement holds. The guest maps as many
            // bytes as it asked for from the placement's offset, so a
            // second MMAP of the same file asking for more than the first
            // would reach past this extent into the window's next ones --
            // another guest process's device memory -- or into unplaced
            // window, whose first touch stops the VM. The recorded path
            // refuses the same (`handle_mmap`).
            let mapped = live.length.div_ceil(4096) * 4096;
            if size > mapped {
                log::warn!(
                    "mmap on handle {handle}: {size:#x} bytes asked of a {mapped:#x}-byte \
                     placement already made at file offset {fd_offset:#x}; refused"
                );
                return self.write_error(resp_buf, libc::EINVAL);
            }
            // A placement made writable before the range was an injected
            // buffer's (a stale id since reused) is not handed out again.
            if live.writable && drm && self.inject.read_only(fd_offset, size.max(4096)) {
                log::warn!(
                    "mmap on handle {handle}: a writable placement at file offset \
                     {fd_offset:#x} is now an injected buffer's; refused"
                );
                return self.write_error(resp_buf, libc::EACCES);
            }
            live.refs += 1;
            let (offset, length) = (live.region.offset, live.length);
            let (pgprot, writable) = (live.region.pgprot, live.writable);
            return self.write_mmap_resp(
                resp_buf,
                offset,
                length.div_ceil(4096) * 4096,
                id,
                pgprot,
                writable,
            );
        }

        let length = size.max(4096);
        // A size the guest kernel chose, refused before any arithmetic on
        // it: no zone holds more than its own size.
        if length > self.shm.largest_zone() {
            log::warn!("mmap on handle {handle}: {length:#x} bytes is more than any zone holds");
            return self.write_error(resp_buf, libc::ENOMEM);
        }
        // An injected buffer's range is placed read-only, whichever of the
        // VM's files maps it (inject.rs): the guest's CPU does not write
        // into the host's capture buffers.
        let injected = drm && self.inject.read_only(fd_offset, length);
        let writable = !injected && crate::shm::host_mapping_writable(host_fd, length, fd_offset);
        let region = match self.alloc_zone(length, pgprot, self.handles.owner(handle)) {
            Ok(r) => r,
            Err(e) => {
                log::error!("mmap on handle {handle}: window has no room: {e}");
                return self.write_error(resp_buf, libc::ENOMEM);
            }
        };

        let Some(window) = self.window.as_ref() else {
            log::warn!("mmap on handle {handle}: no shared window to place it in");
            if let Err(e) = self.shm.free(&region) {
                log::warn!("mmap on handle {handle}: freeing the unused region: {e}");
            }
            return self.write_error(resp_buf, libc::ENOTSUP);
        };
        if let Err(e) = window.place(region.offset, length, host_fd, fd_offset, writable) {
            log::warn!(
                "mmap on handle {handle}: nothing armed on this file, or it could \
                 not be placed: {e}"
            );
            if let Err(e) = self.shm.free(&region) {
                log::warn!("mmap on handle {handle}: freeing the unused region: {e}");
            }
            return self.write_error(resp_buf, libc::EINVAL);
        }

        log::debug!(
            "mmap on handle {handle}: placed {length:#x} bytes at window offset {:#x} \
             with no arming recorded here{}",
            region.offset,
            if injected {
                ", read-only (injected)"
            } else {
                ""
            }
        );

        let (offset, pgprot) = (region.offset, region.pgprot);
        let id = self.next_mapping_id();
        self.dri_maps.insert((handle, fd_offset), id);
        self.live_maps.insert(
            id,
            LiveMap {
                key: Some((handle, fd_offset)),
                region,
                length,
                refs: 1,
                writable,
            },
        );
        self.write_mmap_resp(
            resp_buf,
            offset,
            length.div_ceil(4096) * 4096,
            id,
            pgprot,
            writable,
        )
    }

    /// A mapping id no live placement has. Zero means "none" to the guest.
    pub(super) fn next_mapping_id(&mut self) -> u32 {
        loop {
            let id = self.next_mapping_id;
            self.next_mapping_id = self.next_mapping_id.wrapping_add(1).max(1);
            if !self.live_maps.contains_key(&id)
                && !self.active_maps.has_mapping_id(id)
                && !self.uvm_maps.has_mapping_id(id)
            {
                return id;
            }
        }
    }

    /// An RM mapping has ended on the host -- RM_UNMAP_MEMORY, or the close
    /// of the file carrying it -- and its placement goes with it, unless a
    /// guest vma still maps it: then the extent moves to `live_maps` under the
    /// id those vmas hold, and the last MUNMAP of it releases it (M-3).
    pub(super) fn end_rm_mapping(&mut self, entry: crate::mmap::MmapEntry, why: &str) {
        if entry.refs == 0 {
            self.release_extent(&entry.region, entry.shm_length, why);
            return;
        }
        log::debug!(
            "{why}: window offset {:#x} is still mapped by {} guest vma(s) of id {}; \
             released when they are gone",
            entry.region.offset,
            entry.refs,
            entry.mapping_id
        );
        self.live_maps.insert(
            entry.mapping_id,
            LiveMap {
                key: None,
                region: entry.region,
                length: entry.shm_length,
                refs: entry.refs,
                writable: entry.writable,
            },
        );
    }

    pub(super) fn handle_munmap(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        let Some(req) = pod::read::<MunmapReq>(payload, 0) else {
            return self.write_error(resp_buf, libc::EINVAL);
        };

        // A UVM pool in the aperture: the last of its MMAP replies given back
        // takes it out. Only the handle that mapped it can.
        match self.uvm_maps.munmap(self.current_handle, req.mapping_id) {
            Some(Some(w)) => {
                self.withdraw_uvm(w, "munmap");
                return self.write_hdr(resp_buf, 0, 0);
            }
            Some(None) => return self.write_hdr(resp_buf, 0, 0),
            None => {}
        }

        // An RM mapping still in force: its vmas are counted here, and its
        // extent stays with it until RM_UNMAP_MEMORY or its file's close,
        // which see this count (`end_rm_mapping`).
        if let Some(e) = self.active_maps.find_by_mapping_id_mut(req.mapping_id) {
            e.refs = e.refs.saturating_sub(1);
            return self.write_hdr(resp_buf, 0, 0);
        }
        // Zero is what an older backend handed out for RM mappings, and an
        // unknown id is one already taken back. Reported as success so a guest
        // tearing one down does not log a failure for it.
        let Some(live) = self.live_maps.get_mut(&req.mapping_id) else {
            return self.write_hdr(resp_buf, 0, 0);
        };
        live.refs -= 1;
        if live.refs > 0 {
            return self.write_hdr(resp_buf, 0, 0);
        }
        let live = self
            .live_maps
            .remove(&req.mapping_id)
            .expect("looked up above");
        if let Some(key) = live.key
            && self.dri_maps.get(&key) == Some(&req.mapping_id)
        {
            self.dri_maps.remove(&key);
        }
        self.release_extent(&live.region, live.length, "munmap");
        log::debug!(
            "munmap {}: window offset {:#x}+{:#x} is free again",
            req.mapping_id,
            live.region.offset,
            live.length
        );
        self.write_hdr(resp_buf, 0, 0)
    }
}
